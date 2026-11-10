//! TVA：macOS 语音助手桌面端入口。

mod app;
mod hotkey;
mod model;
mod theme;

use std::sync::Arc;

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, px, size,
};
use tva_agent::Agent;
use tva_core::Config;
use tva_macos::AsrManager;

use crate::app::TvaApp;

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,tva=debug"),
    )
    .init();

    let cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            log::error!("读取配置失败，使用默认配置: {err:#}");
            Config::default()
        }
    };

    // --say "文本"：跳过语音，直接把一段文字当作语音指令执行（调试/无障碍用）。
    let injected: Option<String> = {
        let args: Vec<String> = std::env::args().collect();
        args.iter()
            .position(|arg| arg == "--say")
            .and_then(|index| args.get(index + 1))
            .cloned()
    };

    let (events_tx, events_rx) = async_channel::unbounded();
    let asr = Arc::new(AsrManager::new(cfg.asr.clone(), events_tx.clone()));
    let agent = Arc::new(Agent::new(&cfg, events_tx.clone()));

    let ui = cfg.ui.clone();
    let hotkey_cfg = cfg.hotkey.clone();
    let events_tx_ui = events_tx.clone();

    Application::new().run(move |cx: &mut App| {
        if hotkey_cfg.enabled {
            match hotkey::install(&hotkey_cfg, asr.clone()) {
                Ok(desc) => log::info!("全局快捷键已注册: {desc}"),
                Err(err) => log::warn!("全局快捷键注册失败: {err:#}"),
            }
        }

        let bounds = Bounds::centered(None, size(px(1000.0), px(680.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("TVA 语音助手".into()),
                appears_transparent: true,
                // 让红绿灯与自定义标题栏（44px）垂直居中
                traffic_light_position: Some(gpui::point(px(16.0), px(16.0))),
            }),
            kind: if ui.always_on_top {
                WindowKind::PopUp
            } else {
                WindowKind::Normal
            },
            window_background: WindowBackgroundAppearance::Blurred,
            is_resizable: true,
            ..Default::default()
        };

        let asr_ui = asr.clone();
        let agent_ui = agent.clone();
        cx.open_window(options, move |_window, cx| {
            cx.new(|cx| TvaApp::new(cfg, asr_ui, agent_ui, events_rx, cx))
        })
        .expect("无法创建主窗口");

        cx.activate(true);

        if let Some(text) = injected {
            // 走和真实语音完全相同的事件路径：注入一条 Final 转写结果。
            let events = events_tx_ui.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1200));
                log::info!("注入转写结果: {text}");
                let _ = events.send_blocking(tva_core::AppEvent::Final { text });
            });
        }
    });
}
