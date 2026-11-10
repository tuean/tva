//! 语音识别链路自检：申请权限 -> 打开麦克风 -> 打印实时转写。
//!
//! 用法：cargo run -p tva-macos --example asr_probe [秒数] [语言]

use std::time::Duration;

use tva_core::{AppEvent, AsrConfig};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);
    let locale = args.next().unwrap_or_else(|| "zh-CN".to_string());
    let on_device = std::env::var("TVA_ON_DEVICE").map(|v| v != "0").unwrap_or(true);
    let engine = match std::env::var("TVA_ENGINE").unwrap_or_default().as_str() {
        "whisper" => tva_core::AsrEngine::Whisper,
        _ => tva_core::AsrEngine::Macos,
    };
    let mut cfg = AsrConfig {
        locale,
        on_device,
        engine,
        ..Default::default()
    };
    if let Ok(model) = std::env::var("TVA_MODEL") {
        cfg.model_path = std::path::PathBuf::from(model);
    }
    println!("== TVA ASR 自检 ==");
    println!("引擎: {:?}", cfg.engine);
    if cfg.engine == tva_core::AsrEngine::Macos {
        print!("{}", tva_macos::diagnose(&cfg.locale));
    } else {
        println!("模型: {}", cfg.model_path.display());
    }
    println!(
        "语言: {}，设备端离线: {}，时长: {}s，开始说话…",
        cfg.locale, cfg.on_device, seconds
    );

    let (tx, rx) = async_channel::unbounded();
    let recognizer = tva_macos::Recognizer::spawn(cfg, tx);
    recognizer.start();

    let stop_at = std::time::Instant::now() + Duration::from_secs(seconds);
    let watchdog = std::thread::spawn(move || {
        while std::time::Instant::now() < stop_at {
            std::thread::sleep(Duration::from_millis(100));
        }
        recognizer.stop();
        std::thread::sleep(Duration::from_millis(1500));
    });

    while let Ok(event) = rx.recv_blocking() {
        match event {
            AppEvent::Level { rms } => {
                let bars = (rms * 200.0).clamp(0.0, 40.0) as usize;
                print!("\r电平 {:>6.3} {:<40}", rms, "█".repeat(bars));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            AppEvent::Partial { text } => println!("\n[部分] {text}"),
            AppEvent::Final { text } => println!("\n[最终] {text}"),
            AppEvent::AsrState(state) => println!("\n[状态] {state:?}"),
            AppEvent::AsrError { message } => println!("\n[错误] {message}"),
            other => println!("\n[事件] {other:?}"),
        }
        if std::time::Instant::now() >= stop_at + Duration::from_secs(2) {
            break;
        }
    }
    let _ = watchdog.join();
    println!("\n== 结束 ==");
}