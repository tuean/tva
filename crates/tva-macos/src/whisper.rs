//! 本地 whisper.cpp 引擎（whisper-rs，CPU 推理）。
//!
//! 完全离线：模型是本地 ggml 文件，音频不出机器。
//!
//! 线程模型：
//!
//!   音频线程 ──TapChunk──▶ 识别线程 ──Job{samples}──▶ 解码线程（whisper）
//!                          │                            │
//!                          │ 静音检测 / 断句            └── AppEvent::Partial / Final
//!                          └── AppEvent::Level
//!
//! whisper 不是流式模型，这里的做法是：说话过程中每 ~0.9s 把"整段到目前为止"
//! 重新解码一次当作实时预览（partial），停顿超过 silence_ms 再解码一次作为最终结果。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select};
use tva_core::{AppEvent, AsrConfig, AsrState};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

use crate::Ctrl;
use crate::audio::{AudioInput, VoiceGate};

/// 电平事件发送间隔。
const LEVEL_INTERVAL: Duration = Duration::from_millis(50);
/// 最终结果的最短音频（秒），太短容易出幻觉。
const MIN_DECODE_SECS: f32 = 0.6;
/// 实时预览的最短音频（秒），1 秒以下解出来基本都是垃圾。
const MIN_PARTIAL_SECS: f32 = 1.2;
/// whisper 在静音/噪声上常见的固定幻觉，直接丢掉。
const HALLUCINATIONS: &[&str] = &[
    "谢谢大家",
    "谢谢观看",
    "请不吝点赞",
    "订阅",
    "转发",
    "打赏",
    "字幕由",
    "字幕组",
    "明镜与点点栏目",
    "下次再见",
    "再会",
    "(字幕",
    "（字幕",
    "MING PAO CANADA",
];
/// 空闲轮询间隔。
const TICK: Duration = Duration::from_millis(100);
/// 实时预览最多解码最后多少秒，避免长句子把 CPU 拖垮。
const PARTIAL_WINDOW_SECS: usize = 8;

/// 关掉 whisper.cpp 的启动日志（会直接打到 stdout）。
fn silence_whisper_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        whisper_rs::set_log_callback(Some(quiet_log), std::ptr::null_mut());
    });
}

unsafe extern "C" fn quiet_log(
    _level: std::os::raw::c_uint,
    _text: *const std::os::raw::c_char,
    _user_data: *mut std::ffi::c_void,
) {
}

/// 一次解码任务。
struct Job {
    /// 16kHz 单声道采样。
    samples: Vec<f32>,
    /// true = 这一句已经说完，作为最终结果；false = 实时预览。
    is_final: bool,
}

pub(crate) fn worker(
    cfg: AsrConfig,
    events: async_channel::Sender<AppEvent>,
    _ctrl_tx: Sender<Ctrl>,
    ctrl_rx: Receiver<Ctrl>,
    listening: Arc<AtomicBool>,
) -> Result<()> {
    emit(&events, AppEvent::AsrState(AsrState::Authorizing));

    if !crate::permissions::request_microphone_access(Duration::from_secs(60))
        .context("麦克风权限申请失败")?
    {
        bail!("麦克风权限被拒绝：请在「系统设置 → 隐私与安全性 → 麦克风」中允许 TVA");
    }

    silence_whisper_logs();

    // 最终结果用的大模型。
    let accurate_path = expand_path(&cfg.model_path);
    if !accurate_path.exists() {
        bail!(
            "找不到 whisper 模型：{}\n下载：./scripts/download-model.sh small",
            accurate_path.display(),
        );
    }
    // 实时预览用的快模型（可选）。whisper 每次解码耗时几乎固定，
    // 所以用 tiny 出预览、大模型出最终结果，能明显跟手很多。
    let fast_path = if cfg.partial_model_path.as_os_str().is_empty() {
        accurate_path.clone()
    } else {
        expand_path(&cfg.partial_model_path)
    };
    let two_pass = fast_path != accurate_path && fast_path.exists();

    let input = AudioInput::open()?;

    let accurate = load_context(&accurate_path)?;
    let fast = if two_pass {
        log::info!(
            "双通道识别：预览={} 最终={}",
            fast_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
            accurate_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        );
        Some(load_context(&fast_path)?)
    } else {
        None
    };

    let (job_tx, job_rx) = bounded::<Job>(2);
    let decoding = Arc::new(AtomicBool::new(false));
    {
        let cfg = cfg.clone();
        let events = events.clone();
        let decoding = decoding.clone();
        std::thread::Builder::new()
            .name("tva-whisper".into())
            .spawn(move || {
                if let Err(err) = decode_loop(accurate, fast, &cfg, &events, &job_rx, &decoding) {
                    log::error!("whisper 解码线程退出: {err:#}");
                    let _ = events.send_blocking(AppEvent::AsrError {
                        message: format!("{err:#}"),
                    });
                }
            })?;
    }

    emit(&events, AppEvent::AsrState(AsrState::Idle));

    let mut buffer: Vec<f32> = Vec::with_capacity(16_000 * 20);
    let mut gate = VoiceGate::new(cfg.silence_rms);
    let mut speech_started: Option<Instant> = None;
    let mut last_voice = Instant::now();
    let mut last_level = Instant::now() - LEVEL_INTERVAL;
    let mut last_partial = Instant::now();

    loop {
        select! {
            recv(ctrl_rx) -> msg => {
                let Ok(ctrl) = msg else { break };
                match ctrl {
                    Ctrl::Start => {
                        listening.store(true, Ordering::Relaxed);
                        buffer.clear();
                        speech_started = None;
                        last_voice = Instant::now();
                        last_partial = Instant::now();
                        if let Err(message) = input.start() {
                            emit(&events, AppEvent::AsrError { message: format!("音频引擎启动失败: {message}") });
                            listening.store(false, Ordering::Relaxed);
                            continue;
                        }
                        emit(&events, AppEvent::AsrState(AsrState::Listening));
                        log::info!("开始聆听 (whisper)");
                    }
                    Ctrl::Stop => {
                        // 先把 tap 队列里剩下的音频收干净，避免丢掉最后一两个字。
                        while let Ok(chunk) = input.receiver().try_recv() {
                            buffer.extend_from_slice(&input.to_mono_16k(&chunk));
                        }
                        listening.store(false, Ordering::Relaxed);
                        input.stop();
                        if speech_started.is_some() {
                            finalize(&mut buffer, &job_tx, &events);
                        }
                        speech_started = None;
                        emit(&events, AppEvent::AsrState(AsrState::Idle));
                        log::info!("停止聆听 (whisper)");
                    }
                    Ctrl::Fatal => {
                        listening.store(false, Ordering::Relaxed);
                        input.stop();
                        buffer.clear();
                        emit(&events, AppEvent::AsrState(AsrState::Failed));
                    }
                    Ctrl::Shutdown => break,
                    // whisper 引擎用不到这两个。
                    Ctrl::Restart | Ctrl::Fallback => {}
                }
            }

            recv(input.receiver()) -> chunk => {
                let Ok(chunk) = chunk else { break };
                if !listening.load(Ordering::Relaxed) {
                    continue;
                }
                let now = Instant::now();
                if now.duration_since(last_level) >= LEVEL_INTERVAL {
                    last_level = now;
                    emit(&events, AppEvent::Level { rms: chunk.rms });
                }

                buffer.extend_from_slice(&input.to_mono_16k(&chunk));

                if gate.update(chunk.rms) {
                    last_voice = now;
                    if speech_started.is_none() {
                        speech_started = Some(now);
                        emit(&events, AppEvent::AsrState(AsrState::Recognizing));
                    }
                }

                let speaking = speech_started.is_some();
                let secs = buffer.len() as f32 / 16_000.0;

                // 实时预览：解码线程空闲时才发，避免堆积。
                if speaking
                    && secs >= MIN_PARTIAL_SECS
                    && !decoding.load(Ordering::Relaxed)
                    && now.duration_since(last_partial) >= Duration::from_millis(cfg.partial_interval_ms)
                {
                    last_partial = now;
                    // 预览只看最近的音频，否则长句会越解越慢。
                    let window = PARTIAL_WINDOW_SECS * 16_000;
                    let samples = if buffer.len() > window {
                        buffer[buffer.len() - window..].to_vec()
                    } else {
                        buffer.clone()
                    };
                    if send_job(&job_tx, Job { samples, is_final: false }) {
                        decoding.store(true, Ordering::Relaxed);
                    }
                }

                // 断句：停顿够久 -> 出最终结果，然后开始下一句。
                let long_enough = speech_started
                    .map(|t| now.duration_since(t) >= Duration::from_millis(cfg.min_speech_ms))
                    .unwrap_or(false);
                if long_enough && now.duration_since(last_voice) >= Duration::from_millis(cfg.silence_ms) {
                    finalize(&mut buffer, &job_tx, &events);
                    speech_started = None;
                    emit(&events, AppEvent::AsrState(AsrState::Listening));
                }
            }

            default(TICK) => {}
        }
    }

    input.stop();
    emit(&events, AppEvent::AsrState(AsrState::Idle));
    Ok(())
}

/// 把当前缓冲作为最终结果送去解码，并清空缓冲。
fn finalize(buffer: &mut Vec<f32>, job_tx: &Sender<Job>, events: &async_channel::Sender<AppEvent>) {
    let samples = std::mem::take(buffer);
    if samples.len() as f32 / 16_000.0 < MIN_DECODE_SECS {
        // 太短，直接当空结果，避免误识别。
        emit(events, AppEvent::Final { text: String::new() });
        return;
    }
    if !send_job(job_tx, Job { samples, is_final: true }) {
        log::warn!("whisper 解码队列已满，丢弃一帧最终结果");
    }
}

fn send_job(job_tx: &Sender<Job>, job: Job) -> bool {
    match job_tx.try_send(job) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => false,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// 加载并校验一个 whisper 模型。
fn load_context(path: &std::path::Path) -> Result<WhisperContext> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size < 10 * 1024 * 1024 {
        bail!(
            "whisper 模型文件不完整（{} 只有 {} 字节），可能还在下载或下载中断。请重新运行 ./scripts/download-model.sh",
            path.display(),
            size
        );
    }
    log::info!("加载 whisper 模型: {} ({} MB)", path.display(), size / 1024 / 1024);
    WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .map_err(|err| anyhow!("加载 whisper 模型失败（文件可能损坏）: {err}"))
}

/// 解码线程：独占 whisper state，串行处理任务。
///
/// 最终结果用 accurate，实时预览用 fast（没有 fast 就退化成同一个模型）。
fn decode_loop(
    accurate: WhisperContext,
    fast: Option<WhisperContext>,
    cfg: &AsrConfig,
    events: &async_channel::Sender<AppEvent>,
    job_rx: &Receiver<Job>,
    decoding: &AtomicBool,
) -> Result<()> {
    let mut accurate_state = accurate
        .create_state()
        .map_err(|err| anyhow!("创建 whisper 状态失败: {err}"))?;
    let mut fast_state = match &fast {
        Some(context) => Some(
            context
                .create_state()
                .map_err(|err| anyhow!("创建 whisper 预览状态失败: {err}"))?,
        ),
        None => None,
    };

    while let Ok(job) = job_rx.recv() {
        let started = Instant::now();
        let state = if job.is_final || fast_state.is_none() {
            &mut accurate_state
        } else {
            fast_state.as_mut().expect("fast_state 已判断非空")
        };
        let text = transcribe(state, cfg, &job.samples)?;
        decoding.store(false, Ordering::Relaxed);
        log::debug!(
            "whisper 解码 {} 秒音频用时 {:?}: {text}",
            job.samples.len() as f32 / 16_000.0,
            started.elapsed()
        );
        if job.is_final {
            emit(events, AppEvent::Final { text });
        } else if !text.is_empty() {
            emit(events, AppEvent::Partial { text });
        }
    }
    Ok(())
}

fn transcribe(
    state: &mut whisper_rs::WhisperState,
    cfg: &AsrConfig,
    samples: &[f32],
) -> Result<String> {
    let language = whisper_language(&cfg.locale);
    let threads = if cfg.whisper_threads > 0 {
        cfg.whisper_threads
    } else {
        std::thread::available_parallelism()
            .map(|n| (n.get() as i32).min(8))
            .unwrap_or(4)
    };

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some(&language));
    params.set_translate(false);
    params.set_no_context(true);
    params.set_single_segment(false);
    params.set_suppress_blank(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_n_threads(threads);
    if !cfg.whisper_prompt.trim().is_empty() {
        params.set_initial_prompt(&cfg.whisper_prompt);
    }

    state
        .full(params, samples)
        .map_err(|err| anyhow!("whisper 推理失败: {err}"))?;

    let mut out = String::new();
    for index in 0..state.full_n_segments() {
        let Some(segment) = state.get_segment(index) else {
            continue;
        };
        // 这一段基本是噪声/静音，丢掉，避免 whisper 硬编出文字。
        if segment.no_speech_probability() > 0.6 {
            continue;
        }
        if let Ok(text) = segment.to_str_lossy() {
            out.push_str(&text);
        }
    }
    let text = out.trim().to_string();
    if is_hallucination(&text) {
        return Ok(String::new());
    }
    Ok(text)
}

/// 过滤掉 whisper 在无语音输入时的固定幻觉。
fn is_hallucination(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    // 只由标点/符号组成的结果没有意义。
    if !text.chars().any(|c| c.is_alphanumeric()) {
        return true;
    }
    // 整段都被括号包住，基本是模型在"描述"而非转写。
    let trimmed = text.trim();
    if (trimmed.starts_with('(') && trimmed.ends_with(')'))
        || (trimmed.starts_with('（') && trimmed.ends_with('）'))
    {
        return true;
    }
    HALLUCINATIONS.iter().any(|pattern| text.contains(pattern))
}

/// "zh-CN" -> "zh"，whisper 只认语言代码。
fn whisper_language(locale: &str) -> String {
    locale
        .split(['-', '_'])
        .next()
        .unwrap_or("zh")
        .to_lowercase()
}

/// 供自检工具使用：加载模型并返回可复用的 state。
pub fn load(model_path: &std::path::Path) -> Result<WhisperState> {
    silence_whisper_logs();
    let context = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
        .map_err(|err| anyhow!("加载 whisper 模型失败: {err}"))?;
    context
        .create_state()
        .map_err(|err| anyhow!("创建 whisper 状态失败: {err}"))
}

/// 供自检工具使用：用已有 state 转写一段 16kHz 单声道音频。
pub fn transcribe_state(state: &mut WhisperState, cfg: &AsrConfig, samples: &[f32]) -> Result<String> {
    transcribe(state, cfg, samples)
}

/// 展开 ~ 开头的路径。
fn expand_path(path: &std::path::Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs_home()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn emit(events: &async_channel::Sender<AppEvent>, event: AppEvent) {
    let _ = events.send_blocking(event);
}

#[cfg(test)]
mod tests {
    use super::whisper_language;

    #[test]
    fn locale_to_language() {
        assert_eq!(whisper_language("zh-CN"), "zh");
        assert_eq!(whisper_language("en_US"), "en");
        assert_eq!(whisper_language("ja"), "ja");
    }
}
