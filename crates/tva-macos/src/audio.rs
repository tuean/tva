//! 麦克风采集：AVAudioEngine + tap，把 PCM 变成纯 Rust 采样送给识别线程。
//!
//! 两个识别引擎（系统 Speech / 本地 whisper.cpp）共用这一层。

use std::ptr::NonNull;

use anyhow::{Result, bail};
use block2::RcBlock;
use crossbeam_channel::{Receiver, Sender, bounded};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_avf_audio::{
    AVAudioEngine, AVAudioFormat, AVAudioInputNode, AVAudioPCMBuffer, AVAudioTime,
};
use objc2_foundation::NSError;

/// 每次 tap 回调的音频帧数（约 43ms @48kHz）。
pub const TAP_BUFFER_FRAMES: u32 = 2048;
/// tap 回调 -> 工作线程的队列容量；满了就丢帧，避免阻塞音频线程。
const TAP_QUEUE: usize = 32;

/// 从音频线程传给工作线程的一小块音频。
#[derive(Clone)]
pub struct TapChunk {
    /// 交错（interleaved）时为帧优先，否则为声道优先的连续排列。
    pub samples: Vec<f32>,
    pub frames: usize,
    pub rms: f32,
}

impl TapChunk {
    pub fn duration_secs(&self, sample_rate: f64) -> f32 {
        self.frames as f32 / sample_rate as f32
    }
}

/// 已打开的麦克风输入。
pub struct AudioInput {
    engine: Retained<AVAudioEngine>,
    node: Retained<AVAudioInputNode>,
    format: Retained<AVAudioFormat>,
    channels: usize,
    sample_rate: f64,
    interleaved: bool,
    tap_rx: Receiver<TapChunk>,
}

impl AudioInput {
    /// 打开默认输入设备并装上 tap（此时还没开始收音）。
    pub fn open() -> Result<Self> {
        let engine = unsafe { AVAudioEngine::new() };
        let node = unsafe { engine.inputNode() };
        let format = unsafe { node.outputFormatForBus(0) };
        let channels = unsafe { format.channelCount() } as usize;
        let sample_rate = unsafe { format.sampleRate() };
        let interleaved = unsafe { format.isInterleaved() };
        if channels == 0 || sample_rate <= 0.0 {
            bail!("找不到可用的麦克风输入设备");
        }
        log::info!(
            "音频输入: {} 声道 / {} Hz / {}",
            channels,
            sample_rate,
            if interleaved { "交错" } else { "非交错" }
        );

        let (tap_tx, tap_rx) = bounded::<TapChunk>(TAP_QUEUE);
        let tap_block = RcBlock::new(
            move |buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
                let chunk = unsafe { extract_chunk(buffer.as_ref(), channels, interleaved) };
                let _ = tap_tx.try_send(chunk);
            },
        );
        unsafe {
            node.installTapOnBus_bufferSize_format_block(
                0,
                TAP_BUFFER_FRAMES,
                Some(&format),
                RcBlock::as_ptr(&tap_block),
            );
            engine.prepare();
        }

        Ok(Self {
            engine,
            node,
            format,
            channels,
            sample_rate,
            interleaved,
            tap_rx,
        })
    }

    /// 开始收音（幂等）。
    pub fn start(&self) -> Result<(), String> {
        if unsafe { self.engine.isRunning() } {
            return Ok(());
        }
        unsafe { self.engine.startAndReturnError() }
            .map_err(|err: Retained<NSError>| err.localizedDescription().to_string())
    }

    /// 停止收音。
    pub fn stop(&self) {
        if unsafe { self.engine.isRunning() } {
            unsafe { self.engine.stop() };
        }
    }

    pub fn is_running(&self) -> bool {
        unsafe { self.engine.isRunning() }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 供 select! 使用的采样接收端。
    pub fn receiver(&self) -> &Receiver<TapChunk> {
        &self.tap_rx
    }

    /// 系统 Speech 引擎用：把采样装回 AVAudioPCMBuffer。
    pub fn to_pcm_buffer(&self, chunk: &TapChunk) -> Option<Retained<AVAudioPCMBuffer>> {
        if chunk.frames == 0 {
            return None;
        }
        let buffer = unsafe {
            AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                &self.format,
                chunk.frames as u32,
            )
        }?;
        unsafe { buffer.setFrameLength(chunk.frames as u32) };

        let ptr = unsafe { buffer.floatChannelData() };
        if ptr.is_null() {
            return None;
        }
        let channels = self.channels.max(1);
        unsafe {
            if self.interleaved {
                std::ptr::copy_nonoverlapping(
                    chunk.samples.as_ptr(),
                    (*ptr).as_ptr(),
                    chunk.frames * channels,
                );
            } else {
                for ch in 0..channels {
                    std::ptr::copy_nonoverlapping(
                        chunk.samples.as_ptr().add(ch * chunk.frames),
                        (*ptr.add(ch)).as_ptr(),
                        chunk.frames,
                    );
                }
            }
        }
        Some(buffer)
    }

    /// whisper 引擎用：混成单声道并重采样到 16kHz（whisper 要求）。
    pub fn to_mono_16k(&self, chunk: &TapChunk) -> Vec<f32> {
        let channels = self.channels.max(1);
        let mono: Vec<f32> = if channels == 1 {
            chunk.samples.clone()
        } else if self.interleaved {
            chunk
                .samples
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                .collect()
        } else {
            (0..chunk.frames)
                .map(|i| {
                    (0..channels)
                        .map(|ch| chunk.samples[ch * chunk.frames + i])
                        .sum::<f32>()
                        / channels as f32
                })
                .collect()
        };
        resample_to_16k(&mono, self.sample_rate)
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        unsafe { self.node.removeTapOnBus(0) };
    }
}

/// 把任意采样率降/升到 16kHz。整数倍用盒式平均（顺带抗混叠），否则线性插值。
pub fn resample_to_16k(input: &[f32], from_rate: f64) -> Vec<f32> {
    const TARGET: f64 = 16_000.0;
    if (from_rate - TARGET).abs() < 1.0 || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate / TARGET;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);

    if (ratio - ratio.round()).abs() < 1e-9 && ratio >= 1.0 {
        let step = ratio.round() as usize;
        for index in 0..out_len {
            let start = index * step;
            let end = (start + step).min(input.len());
            if start >= input.len() {
                break;
            }
            let sum: f32 = input[start..end].iter().sum();
            out.push(sum / (end - start) as f32);
        }
    } else {
        for index in 0..out_len {
            let position = index as f64 * ratio;
            let index = position as usize;
            let frac = (position - index as f64) as f32;
            let a = input.get(index).copied().unwrap_or(0.0);
            let b = input.get(index + 1).copied().unwrap_or(a);
            out.push(a + (b - a) * frac);
        }
    }
    out
}

/// 在音频线程上把 AVAudioPCMBuffer 拷贝成纯 Rust 数据并算 RMS。
///
/// # Safety
/// buffer 必须由 tap 回调提供，且 channels / interleaved 与它的格式一致。
unsafe fn extract_chunk(buffer: &AVAudioPCMBuffer, channels: usize, interleaved: bool) -> TapChunk {
    let frames = unsafe { buffer.frameLength() } as usize;
    let channels = channels.max(1);
    let mut samples = vec![0f32; frames * channels];
    let ptr = unsafe { buffer.floatChannelData() };
    if ptr.is_null() || frames == 0 {
        return TapChunk {
            samples,
            frames,
            rms: 0.0,
        };
    }

    let mut sum_sq = 0f64;
    unsafe {
        if interleaved {
            std::ptr::copy_nonoverlapping((*ptr).as_ptr(), samples.as_mut_ptr(), frames * channels);
            for frame in samples.chunks_exact(channels) {
                let mono = frame.iter().sum::<f32>() / channels as f32;
                sum_sq += (mono as f64) * (mono as f64);
            }
        } else {
            for ch in 0..channels {
                std::ptr::copy_nonoverlapping(
                    (*ptr.add(ch)).as_ptr(),
                    samples.as_mut_ptr().add(ch * frames),
                    frames,
                );
            }
            for i in 0..frames {
                let mut mono = 0f32;
                for ch in 0..channels {
                    mono += samples[ch * frames + i];
                }
                mono /= channels as f32;
                sum_sq += (mono as f64) * (mono as f64);
            }
        }
    }

    let rms = (sum_sq / frames as f64).sqrt() as f32;
    TapChunk {
        samples,
        frames,
        rms,
    }
}

/// 预留：需要把 Sender 交给别处时的类型别名。
pub type TapSender = Sender<TapChunk>;

/// 自适应语音门限。
///
/// 固定阈值在不同麦克风上很难用：底噪高的环境会一直误触发，灵敏度低的麦克风
/// 又永远触发不了。这里跟踪噪声底噪，只在底噪确实偏高时才抬高阈值：
///
///   阈值 = clamp(底噪 * 1.5, 配置值, 0.08)
///
/// 也就是说配置里的 silence_rms 是"最低灵敏度"，环境安静时行为不变。
pub struct VoiceGate {
    noise_floor: f32,
    floor: f32,
    threshold: f32,
    voiced: bool,
}

impl VoiceGate {
    pub fn new(configured: f32) -> Self {
        let floor = configured.max(1e-4);
        Self {
            noise_floor: floor,
            floor,
            threshold: floor,
            voiced: false,
        }
    }

    /// 当前阈值（界面可以显示）。
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// 喂一帧 RMS，返回这一帧是否算"有人在说话"。
    pub fn update(&mut self, rms: f32) -> bool {
        // 底噪：下降快、上升慢，避免把一句话本身当成噪声。
        if rms < self.noise_floor {
            self.noise_floor = self.noise_floor * 0.8 + rms * 0.2;
        } else {
            self.noise_floor = self.noise_floor * 0.9995 + rms * 0.0005;
        }
        self.threshold = (self.noise_floor * 1.5).clamp(self.floor, 0.08);

        // 一点滞回，避免在阈值附近抖动。
        let enter = rms > self.threshold;
        let stay = rms > self.threshold * 0.6;
        self.voiced = if self.voiced { stay } else { enter };
        self.voiced
    }
}

#[cfg(test)]
mod gate_tests {
    use super::VoiceGate;

    #[test]
    fn quiet_room_keeps_configured_floor() {
        let mut gate = VoiceGate::new(0.012);
        for _ in 0..200 {
            assert!(!gate.update(0.008));
        }
        assert!((gate.threshold() - 0.012).abs() < 1e-6);
        assert!(gate.update(0.05));
    }

    #[test]
    fn noisy_room_raises_threshold() {
        let mut gate = VoiceGate::new(0.012);
        for _ in 0..3000 {
            gate.update(0.03);
        }
        assert!(gate.threshold() > 0.03, "阈值应随底噪抬高: {}", gate.threshold());
        assert!(!gate.update(0.02));
        assert!(gate.update(0.08));
    }

    #[test]
    fn hysteresis_holds_while_speaking() {
        let mut gate = VoiceGate::new(0.01);
        for _ in 0..100 {
            gate.update(0.005);
        }
        assert!(gate.update(0.04));
        // 掉到阈值下方但仍在 60% 以上，应该继续保持说话状态。
        assert!(gate.update(0.011));
    }
}

#[cfg(test)]
mod tests {
    use super::resample_to_16k;

    #[test]
    fn downsample_48k_is_box_average() {
        let input: Vec<f32> = (0..48).map(|i| i as f32).collect();
        let out = resample_to_16k(&input, 48_000.0);
        assert_eq!(out.len(), 16);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!((out[1] - 4.0).abs() < 1e-6);
    }

    #[test]
    fn passthrough_at_16k() {
        let input = vec![0.5f32; 100];
        assert_eq!(resample_to_16k(&input, 16_000.0), input);
    }

    #[test]
    fn upsample_8k_doubles_length() {
        let input = vec![1.0f32; 100];
        let out = resample_to_16k(&input, 8_000.0);
        assert_eq!(out.len(), 200);
        assert!(out.iter().all(|v| (*v - 1.0).abs() < 1e-6));
    }
}