//! Agent 链路自检：把一条文本当作语音指令，打印整个执行过程。
//!
//! 用法：cargo run -p tva-agent --example agent_probe -- "列出桌面上的文件"

use std::time::Duration;

use tva_core::Config;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let prompt = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    let prompt = if prompt.trim().is_empty() {
        "说一句话：你好，我是 TVA".to_string()
    } else {
        prompt
    };

    let cfg = Config::default();
    let (tx, rx) = async_channel::unbounded();
    let agent = tva_agent::Agent::new(&cfg, tx);

    println!("== TVA Agent 自检 ==");
    println!("后端: {}", agent.describe());
    println!("指令: {prompt}\n");

    agent.handle_utterance(&prompt);

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        match rx.recv_blocking() {
            Ok(event) => {
                match event {
                    tva_core::AppEvent::Command { id, route, .. } => {
                        println!("[指令 #{id}] 路由={}", route.label())
                    }
                    tva_core::AppEvent::AgentDelta { text, .. } => print!("{text}"),
                    tva_core::AppEvent::AgentText { text, .. } => println!("\n[回复] {text}"),
                    tva_core::AppEvent::ActionStart { action, name, detail, .. } => {
                        println!("\n[动作 {:?}] {name} :: {detail}", action)
                    }
                    tva_core::AppEvent::ActionOutput { chunk, .. } => print!("{chunk}"),
                    tva_core::AppEvent::ActionEnd { ok, summary, elapsed, .. } => {
                        println!("\n[动作结束] ok={ok} {summary} ({:?})", elapsed)
                    }
                    tva_core::AppEvent::CommandDone { id, ok } => {
                        println!("\n[完成] #{id} ok={ok}");
                        break;
                    }
                    tva_core::AppEvent::Error { message } => println!("\n[错误] {message}"),
                    other => println!("\n[事件] {other:?}"),
                }
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            Err(_) => break,
        }
    }
    println!("\n== 结束 ==");
}
