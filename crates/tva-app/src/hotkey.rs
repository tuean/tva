//! 全局快捷键：按住说话（Pressed/Released）与开/关切换。
//!
//! macOS 上走 Carbon RegisterEventHotKey，不需要辅助功能权限；但必须在
//! 主线程、且 NSApplication 的事件循环已启动后创建，所以由 main 在
//! Application::run 回调里调用 install。

use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tva_core::HotkeyConfig;
use tva_macos::AsrManager;

/// 管理器必须活到进程结束，注册才会持续生效。
static MANAGER: OnceLock<GlobalHotKeyManager> = OnceLock::new();

/// 注册快捷键并启动监听线程。返回给日志用的描述。
pub fn install(cfg: &HotkeyConfig, asr: Arc<AsrManager>) -> Result<String> {
    let manager = GlobalHotKeyManager::new().context("创建全局快捷键管理器失败")?;

    let push_to_talk = HotKey::from_str(&cfg.push_to_talk)
        .with_context(|| format!("无法解析快捷键 {}", cfg.push_to_talk))?;
    let toggle = HotKey::from_str(&cfg.toggle)
        .with_context(|| format!("无法解析快捷键 {}", cfg.toggle))?;

    manager
        .register(push_to_talk)
        .with_context(|| format!("注册 {} 失败（可能被其它 App 占用）", cfg.push_to_talk))?;
    manager
        .register(toggle)
        .with_context(|| format!("注册 {} 失败（可能被其它 App 占用）", cfg.toggle))?;

    let _ = MANAGER.set(manager);

    let ptt_id = push_to_talk.id();
    let toggle_id = toggle.id();
    std::thread::Builder::new()
        .name("tva-hotkey".into())
        .spawn(move || {
            let receiver = GlobalHotKeyEvent::receiver().clone();
            while let Ok(event) = receiver.recv() {
                log::debug!("全局快捷键事件: id={} state={:?}", event.id(), event.state());
                if event.id() == ptt_id {
                    match event.state() {
                        HotKeyState::Pressed => asr.start(),
                        HotKeyState::Released => asr.stop(),
                    }
                } else if event.id() == toggle_id && event.state() == HotKeyState::Pressed {
                    asr.toggle();
                }
            }
        })?;

    Ok(format!("{} / {}", cfg.push_to_talk, cfg.toggle))
}
