//! 执行后端自检：可指定 pi / dsh / opencli / shell，打印完整事件流。
//!
//! 用法：cargo run -p tva-agent --example backend_probe -- <backend> "<指令>"

use std::time::Duration;

use tva_agent::AgentPreset;
use tva_core::Config;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let backend = args.next().unwrap_or_else(|| "pi".to_string());
    let prompt = {
        let rest: Vec<String> = args.collect();
        if rest.is_empty() {
            "只回复四个字：链路正常".to_string()
        } else {
            rest.join(" ")
        }
    };

    let preset = match backend.as_str() {
        "dsh" => AgentPreset::Dsh,
        "opencli" => AgentPreset::Opencli,
        "shell" => AgentPreset::Shell,
        _ => AgentPreset::Pi,
    };

    let mut cfg = Config::default();
    preset.apply(&mut cfg.agent);
    cfg.agent.timeout_secs = 180;

    let (tx, rx) = async_channel::unbounded();
    let agent = tva_agent::Agent::new(&cfg, tx);

    println!("== 后端自检 ==");
    println!("后端: {} ({})", preset.label(), preset.hint());
    println!("实际: {}", agent.describe());
    println!("指令: {prompt}\n");

    // 传 "switch" 时验证运行时切换：pi -> dsh -> pi。
    if backend == "switch" {
        for preset in [AgentPreset::Pi, AgentPreset::Dsh, AgentPreset::Pi] {
            let mut cfg = agent.config();
            preset.apply(&mut cfg);
            match agent.switch(cfg) {
                Ok(()) => println!("切换到 {} -> {}", preset.label(), agent.describe()),
                Err(err) => println!("切换 {} 失败: {err:#}", preset.label()),
            }
        }
        println!("== 切换测试结束 ==");
        return;
    }

    let started = std::time::Instant::now();
    agent.handle_utterance(&prompt);

    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    while std::time::Instant::now() < deadline {
        match rx.recv_blocking() {
            Ok(event) => match event {
                tva_core::AppEvent::Stage { stage, .. } => {
                    println!("[{:>8.1}s] 阶段: {stage:?}", started.elapsed().as_secs_f32());
                }
                tva_core::AppEvent::ActionStart { name, detail, .. } => {
                    println!("[{:>8.1}s] 动作开始: {name} :: {detail}", started.elapsed().as_secs_f32());
                }
                tva_core::AppEvent::ActionOutput { chunk, .. } => {
                    print!("{chunk}");
                }
                tva_core::AppEvent::ActionEnd { ok, summary, elapsed, .. } => {
                    println!("[{:>8.1}s] 动作结束: ok={ok} {summary} ({elapsed:?})", started.elapsed().as_secs_f32());
                }
                tva_core::AppEvent::AgentText { text, .. } => {
                    println!("[{:>8.1}s] 回复: {text}", started.elapsed().as_secs_f32());
                }
                tva_core::AppEvent::CommandDone { ok, .. } => {
                    println!("[{:>8.1}s] 完成: ok={ok}", started.elapsed().as_secs_f32());
                    break;
                }
                tva_core::AppEvent::Error { message } => println!("[错误] {message}"),
                _ => {}
            },
            Err(_) => break,
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    println!("\n== 结束（总耗时 {:?}）==", started.elapsed());
}
