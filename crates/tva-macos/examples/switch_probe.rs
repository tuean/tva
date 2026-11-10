//! 运行时切换识别引擎的自检。
//!
//! 依次在 系统 Speech / whisper 极速 / whisper 精准 之间切换，打印每次的状态事件。

use std::time::Duration;

use tva_core::{AppEvent, AsrConfig, AsrPreset};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (tx, rx) = async_channel::unbounded();
    let cfg = AsrConfig::default();
    let manager = std::sync::Arc::new(tva_macos::AsrManager::new(cfg, tx));

    std::thread::spawn({
        let manager = manager.clone();
        move || {
            let plan = [
                (AsrPreset::MacosSpeech, "系统 Speech"),
                (AsrPreset::WhisperTiny, "whisper 极速"),
                (AsrPreset::WhisperBase, "whisper 标准"),
                (AsrPreset::WhisperSmall, "whisper 精准"),
            ];
            for (preset, label) in plan {
                if !preset.ready() {
                    println!("跳过 {label}: 模型缺失 {:?}", preset.required_models());
                    continue;
                }
                let mut cfg = manager.config();
                preset.apply(&mut cfg);
                println!("\n>>> 切换到 {label} …");
                match manager.switch(cfg) {
                    Ok(()) => {
                        manager.start();
                        std::thread::sleep(Duration::from_secs(4));
                        manager.stop();
                        std::thread::sleep(Duration::from_millis(800));
                    }
                    Err(err) => println!("切换失败: {err:#}"),
                }
            }
            println!("\n>>> 全部切换完成");
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while std::time::Instant::now() < deadline {
        match rx.recv_blocking() {
            Ok(AppEvent::AsrState(state)) => println!("[状态] {state:?}"),
            Ok(AppEvent::Level { .. }) => {}
            Ok(AppEvent::Error { message }) => println!("[提示] {message}"),
            Ok(AppEvent::AsrError { message }) => println!("[错误] {message}"),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    println!("== 结束 ==");
}
