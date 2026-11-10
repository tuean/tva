//! 直接对一个音频文件跑本地 whisper，用来验证模型与解码参数。
//!
//! 用法：
//!   cargo run -p tva-macos --example whisper_file -- 音频文件 [模型路径]
//!
//! 非 .f32 的文件会用 ffmpeg 转成 16kHz 单声道 float32 再喂给 whisper。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tva_core::AsrConfig;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let audio = args.next().unwrap_or_else(|| {
        eprintln!("用法: whisper_file <音频文件> [模型路径]");
        std::process::exit(2);
    });
    let model = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(tva_core::default_model_path);

    let cfg = AsrConfig::default();
    let mut samples = match load_audio(Path::new(&audio)) {
        Ok(samples) => samples,
        Err(err) => {
            eprintln!("读取音频失败: {err:#}");
            std::process::exit(1);
        }
    };

    // 可选：模拟从麦克风拿到的原始采样率，验证 tva 自己的重采样实现。
    if let Ok(rate) = std::env::var("TVA_RESAMPLE_FROM") {
        let rate: f64 = rate.parse().unwrap_or(48_000.0);
        println!("重采样: {rate} Hz -> 16000 Hz（tva 内置实现）");
        samples = tva_macos::audio::resample_to_16k(&samples, rate);
    }

    println!("音频: {audio}（{:.2} 秒）", samples.len() as f32 / 16_000.0);
    println!("模型: {}", model.display());

    let repeats: usize = std::env::var("TVA_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);

    let started = std::time::Instant::now();
    let mut state = match tva_macos::whisper::load(&model) {
        Ok(state) => state,
        Err(err) => {
            eprintln!("加载模型失败: {err:#}");
            std::process::exit(1);
        }
    };
    println!("模型加载: {:?}", started.elapsed());

    for round in 0..repeats {
        let started = std::time::Instant::now();
        match tva_macos::whisper::transcribe_state(&mut state, &cfg, &samples) {
            Ok(text) => {
                let elapsed = started.elapsed();
                let secs = samples.len() as f32 / 16_000.0;
                println!(
                    "解码 #{}: {:?}（{:.2}x 实时）  转写: {text}",
                    round + 1,
                    elapsed,
                    secs as f64 / elapsed.as_secs_f64()
                );
            }
            Err(err) => {
                eprintln!("转写失败: {err:#}");
                std::process::exit(1);
            }
        }
    }
}

/// 读取音频为 16kHz 单声道 f32。
fn load_audio(path: &Path) -> anyhow::Result<Vec<f32>> {
    if path.extension().and_then(|e| e.to_str()) == Some("f32") {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut bytes)?;
        return Ok(bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect());
    }

    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-ar", "16000", "-ac", "1", "-f", "f32le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        anyhow::bail!("ffmpeg 转换失败");
    }
    Ok(output
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}