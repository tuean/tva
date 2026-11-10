//! pi 的 RPC 后端（--mode rpc，JSONL over stdio）。
//!
//! 协议参考 pi 包内 docs/rpc.md：stdin 逐行发命令，stdout 逐行收事件。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tva_core::{ActionId, AgentConfig, AgentStage, AppEvent};

use crate::{AgentBackend, EventSink};

/// 长驻的 pi RPC 进程。
pub struct PiRpc {
    child: Child,
    stdin: ChildStdin,
    current: Arc<AtomicU64>,
    events: EventSink,
}

impl PiRpc {
    /// 启动 pi --mode rpc 并接管它的 stdout。
    pub fn spawn(cfg: &AgentConfig, events: EventSink) -> Result<Self> {
        let mut cmd = Command::new(&cfg.pi_command);
        cmd.args(&cfg.pi_args);
        if !cfg.system_prompt.trim().is_empty() {
            cmd.arg("--append-system-prompt").arg(&cfg.system_prompt);
        }
        cmd.current_dir(&cfg.working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 {} 失败（是否已安装并在 PATH 中？）", cfg.pi_command))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("无法获取 pi 的 stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法获取 pi 的 stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("无法获取 pi 的 stderr"))?;

        std::thread::Builder::new()
            .name("tva-pi-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    log::debug!("[pi stderr] {line}");
                }
            })?;

        let current = Arc::new(AtomicU64::new(0));
        let reader_current = current.clone();
        let reader_events = events.clone();
        std::thread::Builder::new()
            .name("tva-pi-events".into())
            .spawn(move || read_events(stdout, reader_events, reader_current))?;

        Ok(Self {
            child,
            stdin,
            current,
            events,
        })
    }
}

impl AgentBackend for PiRpc {
    fn submit(&mut self, id: u64, prompt: &str) -> Result<()> {
        self.current.store(id, Ordering::Relaxed);
        let request = json!({
            "id": format!("tva-{id}"),
            "type": "prompt",
            "message": prompt,
        });
        writeln!(self.stdin, "{request}").context("写入 pi stdin 失败")?;
        self.stdin.flush().context("刷新 pi stdin 失败")?;
        Ok(())
    }

    fn interrupt(&mut self) {
        let _ = writeln!(self.stdin, "{}", json!({ "type": "abort" }));
        let _ = self.stdin.flush();
    }

    fn describe(&self) -> String {
        format!("pi --mode rpc (pid {})", self.child.id())
    }
}

impl Drop for PiRpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.events.send_blocking(AppEvent::Error {
            message: "pi RPC 进程已关闭".into(),
        });
    }
}

/// 读取 pi 的事件流并翻译成 UI 事件。
fn read_events(stdout: ChildStdout, events: EventSink, current: Arc<AtomicU64>) {
    let reader = BufReader::new(stdout);
    let mut actions: HashMap<String, ActionId> = HashMap::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    let mut starts: HashMap<String, std::time::Instant> = HashMap::new();
    let mut next_action = 0u64;
    // 同一个阶段只发一次，避免每个 token 都推一条事件。
    let mut last_stage: Option<AgentStage> = None;

    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            log::debug!("[pi] 非 JSON 行: {line}");
            continue;
        };
        let id = current.load(Ordering::Relaxed);
        match value["type"].as_str().unwrap_or_default() {
            "agent_start" => {
                stage_once(&events, id, AgentStage::Started, &mut last_stage);
            }
            "message_update" => {
                let event = &value["assistantMessageEvent"];
                match event["type"].as_str().unwrap_or_default() {
                    "text_start" | "text_delta" => {
                        stage_once(&events, id, AgentStage::Responding, &mut last_stage);
                        if let Some(delta) = event["delta"].as_str() {
                            emit(&events, AppEvent::AgentDelta { id, text: delta.to_string() });
                        }
                    }
                    "thinking_start" | "thinking_delta" | "toolcall_start" => {
                        stage_once(&events, id, AgentStage::Reasoning, &mut last_stage);
                    }
                    _ => {}
                }
            }
            "message_end" => {
                let message = &value["message"];
                if message["role"].as_str() == Some("assistant")
                    && let Some(text) = content_text(&message["content"])
                    && !text.is_empty()
                {
                    emit(&events, AppEvent::AgentText { id, text });
                }
            }
            "tool_execution_start" => {
                let call_id = value["toolCallId"].as_str().unwrap_or_default().to_string();
                let tool_name = value["toolName"].as_str().unwrap_or("tool").to_string();
                stage_once(
                    &events,
                    id,
                    AgentStage::Tool {
                        name: tool_name.clone(),
                    },
                    &mut last_stage,
                );
                next_action += 1;
                let action = ActionId(id.wrapping_mul(1000).wrapping_add(next_action));
                actions.insert(call_id.clone(), action);
                outputs.remove(&call_id);
                starts.insert(call_id.clone(), std::time::Instant::now());
                emit(
                    &events,
                    AppEvent::ActionStart {
                        id,
                        action,
                        name: value["toolName"].as_str().unwrap_or("tool").to_string(),
                        detail: summarize(&value["args"]),
                    },
                );
            }
            "tool_execution_update" => {
                let call_id = value["toolCallId"].as_str().unwrap_or_default();
                if let Some(action) = actions.get(call_id).copied() {
                    let text = content_text(&value["partialResult"]["content"]).unwrap_or_default();
                    let previous = outputs.get(call_id).cloned().unwrap_or_default();
                    if text.len() > previous.len() {
                        let delta = text[previous.len()..].to_string();
                        outputs.insert(call_id.to_string(), text);
                        emit(&events, AppEvent::ActionOutput { id, action, chunk: delta });
                    }
                }
            }
            "tool_execution_end" => {
                let call_id = value["toolCallId"].as_str().unwrap_or_default();
                stage_once(
                    &events,
                    id,
                    AgentStage::ToolDone {
                        name: value["toolName"].as_str().unwrap_or("tool").to_string(),
                    },
                    &mut last_stage,
                );
                if let Some(action) = actions.get(call_id).copied() {
                    let ok = !value["isError"].as_bool().unwrap_or(false);
                    let elapsed = starts
                        .remove(call_id)
                        .map(|start| start.elapsed())
                        .unwrap_or(Duration::ZERO);
                    emit(
                        &events,
                        AppEvent::ActionEnd {
                            id,
                            action,
                            ok,
                            summary: summarize(&value["result"]),
                            elapsed,
                        },
                    );
                }
            }
            "agent_settled" => {
                stage_once(&events, id, AgentStage::Settled, &mut last_stage);
                emit(&events, AppEvent::CommandDone { id, ok: true });
            }
            "extension_error" => {
                emit(
                    &events,
                    AppEvent::Error {
                        message: format!("pi 扩展错误: {}", summarize(&value)),
                    },
                );
            }
            _ => {}
        }
    }

    emit(
        &events,
        AppEvent::Error {
            message: "pi RPC 进程已退出（检查 pi 是否可用）".into(),
        },
    );
}

/// 把 content 数组里的文本拼起来。
fn content_text(content: &Value) -> Option<String> {
    let array = content.as_array()?;
    let mut out = String::new();
    for item in array {
        if let Some(text) = item["text"].as_str() {
            out.push_str(text);
        }
    }
    Some(out)
}

/// 给界面用的简短描述。兼容 {content:[{type:text,text:..}]} 结构。
fn summarize(value: &Value) -> String {
    if let Some(command) = value["command"].as_str() {
        return truncate(command, 300);
    }
    let content = if value["content"].is_array() {
        &value["content"]
    } else {
        value
    };
    if let Some(text) = content_text(content)
        && !text.trim().is_empty()
    {
        return truncate(text.trim(), 300);
    }
    if value.is_null() {
        return String::new();
    }
    truncate(&value.to_string(), 300)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn emit(events: &EventSink, event: AppEvent) {
    let _ = events.send_blocking(event);
}

/// 只在阶段真正变化时推事件。
fn stage_once(
    events: &EventSink,
    id: u64,
    stage: AgentStage,
    last: &mut Option<AgentStage>,
) {
    if last.as_ref() == Some(&stage) {
        return;
    }
    *last = Some(stage.clone());
    emit(events, AppEvent::Stage { id, stage });
}
