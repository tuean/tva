//! Agent 桥接层：把一条自然语言指令交给本地的 pi / dsh / opencli，并把执行过程
//! 以事件形式回吐给界面。
//!
//! - pi：走 --mode rpc 的 JSONL 协议，长驻进程、流式文本 + 工具调用事件（首选）。
//! - dsh / opencli / shell：一次性子进程，逐行把 stdout 作为动作输出。

pub mod cli;
pub mod intent;
pub mod pi;

pub use intent::{Decision, LocalAction};

/// 执行后端预设：界面上可以一键切换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPreset {
    /// pi --mode rpc：长驻进程，有流式文本和工具调用事件。
    Pi,
    /// dsh --profile headless：一次性任务，只输出最终答复。
    Dsh,
    /// opencli 子命令。
    Opencli,
    /// 把指令直接当 shell 命令执行。
    Shell,
}

impl AgentPreset {
    /// 界面展示顺序。
    pub const ALL: &'static [AgentPreset] = &[
        AgentPreset::Pi,
        AgentPreset::Dsh,
        AgentPreset::Opencli,
        AgentPreset::Shell,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            AgentPreset::Pi => "pi",
            AgentPreset::Dsh => "dsh",
            AgentPreset::Opencli => "opencli",
            AgentPreset::Shell => "shell",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            AgentPreset::Pi => "pi --mode rpc：长驻进程，流式文本 + 工具调用事件（最详细）",
            AgentPreset::Dsh => "dsh --profile headless：一次性任务，只在结束时输出最终答复",
            AgentPreset::Opencli => "opencli：一次性子进程",
            AgentPreset::Shell => "把指令直接当 shell 命令执行（危险）",
        }
    }

    pub fn backend(&self) -> Backend {
        match self {
            AgentPreset::Pi => Backend::Pi,
            AgentPreset::Dsh => Backend::Dsh,
            AgentPreset::Opencli => Backend::Opencli,
            AgentPreset::Shell => Backend::Shell,
        }
    }

    /// 是否能看到中间过程（工具调用等）。
    pub fn streams_progress(&self) -> bool {
        matches!(self, AgentPreset::Pi)
    }

    pub fn apply(&self, cfg: &mut AgentConfig) {
        cfg.backend = self.backend();
    }

    pub fn matches(&self, cfg: &AgentConfig) -> bool {
        cfg.backend == self.backend()
    }
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use parking_lot::Mutex;
use tva_core::{AgentConfig, AgentStage, AppEvent, Backend, Config, IdGen, Route};

/// 事件出口（UI 线程读取）。
pub type EventSink = async_channel::Sender<AppEvent>;

/// 一个可以执行指令的后端。
pub trait AgentBackend: Send {
    /// 提交一条指令。事件通过构造时注入的 sink 流出。
    fn submit(&mut self, id: u64, prompt: &str) -> Result<()>;
    /// 打断当前执行。
    fn interrupt(&mut self);
    /// 供界面显示的后端描述。
    fn describe(&self) -> String;
}

/// 指令执行入口：负责路由（本地动作 vs Agent）与生命周期。
pub struct Agent {
    backend: Mutex<Box<dyn AgentBackend>>,
    ids: IdGen,
    current: AtomicU64,
    events: EventSink,
    cfg: Mutex<AgentConfig>,
    /// shell 类动作是否需要人工确认。
    confirm_shell: bool,
    /// 等待确认的动作。
    pending: Mutex<HashMap<u64, LocalAction>>,
}

impl Agent {
    /// 按配置创建后端。
    pub fn new(cfg: &Config, events: EventSink) -> Self {
        let (backend, note) = match build_backend(&cfg.agent, events.clone()) {
            Ok(backend) => (backend, None),
            Err(err) => (
                Box::new(cli::CliBackend::new(
                    &cfg.agent.dsh_command,
                    &cfg.agent.dsh_args,
                    &cfg.agent.working_dir,
                    std::time::Duration::from_secs(cfg.agent.timeout_secs),
                    events.clone(),
                )) as Box<dyn AgentBackend>,
                Some(format!("{err:#}")),
            ),
        };
        if let Some(note) = note {
            let _ = events.send_blocking(AppEvent::Error {
                message: format!("Agent 后端初始化失败，已回退到 dsh：{note}"),
            });
        }
        Self {
            backend: Mutex::new(backend),
            ids: IdGen::default(),
            current: AtomicU64::new(0),
            events,
            cfg: Mutex::new(cfg.agent.clone()),
            confirm_shell: cfg.ui.confirm_shell,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 后端描述。
    pub fn describe(&self) -> String {
        self.backend.lock().describe()
    }

    /// 当前后端配置。
    pub fn config(&self) -> AgentConfig {
        self.cfg.lock().clone()
    }

    /// 运行时切换执行后端（pi / dsh / opencli / shell）。
    pub fn switch(&self, cfg: AgentConfig) -> Result<()> {
        // 先按新配置把后端建起来，建失败就保持原样。
        let new_backend = build_backend(&cfg, self.events.clone())?;
        let old = std::mem::replace(&mut *self.backend.lock(), new_backend);
        // 旧后端在这里被丢弃：pi 的常驻进程会被 kill。
        drop(old);
        *self.cfg.lock() = cfg;
        log::info!("执行后端已切换: {}", self.describe());
        Ok(())
    }

    /// 当前正在执行的指令 id。
    pub fn current_id(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }

    /// 处理一段最终转写文本：先尝试本地意图，否则交给 Agent。
    pub fn handle_utterance(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let id = self.ids.next();

        match intent::decide(text) {
            Decision::Local(action) => {
                let _ = self.events.send_blocking(AppEvent::Command {
                    id,
                    text: text.to_string(),
                    route: Route::Local,
                });
                if self.confirm_shell && matches!(action, LocalAction::Shell(_)) {
                    let detail = match &action {
                        LocalAction::Shell(cmd) => cmd.clone(),
                        _ => String::new(),
                    };
                    self.pending.lock().insert(id, action);
                    let _ = self.events.send_blocking(AppEvent::ConfirmRequest {
                        id,
                        text: text.to_string(),
                        detail,
                    });
                    return;
                }
                let events = self.events.clone();
                std::thread::spawn(move || action.execute(id, events));
            }
            Decision::Agent => {
                let _ = self.events.send_blocking(AppEvent::Command {
                    id,
                    text: text.to_string(),
                    route: Route::Agent,
                });
                self.current.store(id, Ordering::Relaxed);
                let _ = self.events.send_blocking(AppEvent::Stage {
                    id,
                    stage: AgentStage::Submitted,
                });
                if let Err(err) = self.backend.lock().submit(id, text) {
                    let _ = self.events.send_blocking(AppEvent::Error {
                        message: format!("提交给 Agent 失败: {err:#}"),
                    });
                    let _ = self.events.send_blocking(AppEvent::CommandDone { id, ok: false });
                }
            }
        }
    }

    /// 确认执行一条等待确认的本地动作。
    pub fn confirm(&self, id: u64) {
        if let Some(action) = self.pending.lock().remove(&id) {
            let events = self.events.clone();
            std::thread::spawn(move || action.execute(id, events));
        }
    }

    /// 取消一条等待确认的本地动作。
    pub fn cancel(&self, id: u64) {
        if self.pending.lock().remove(&id).is_some() {
            let _ = self.events.send_blocking(AppEvent::CommandDone { id, ok: false });
        }
    }

    /// 打断当前执行。
    pub fn interrupt(&self) {
        self.backend.lock().interrupt();
    }

    /// 超时时间。
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.cfg.lock().timeout_secs)
    }
}

fn build_backend(cfg: &AgentConfig, events: EventSink) -> Result<Box<dyn AgentBackend>> {
    let timeout = std::time::Duration::from_secs(cfg.timeout_secs);
    Ok(match cfg.backend {
        Backend::Pi => Box::new(pi::PiRpc::spawn(cfg, events)?),
        Backend::Dsh => Box::new(cli::CliBackend::new(
            &cfg.dsh_command,
            &cfg.dsh_args,
            &cfg.working_dir,
            timeout,
            events,
        )),
        Backend::Opencli => Box::new(cli::CliBackend::new(
            &cfg.opencli_command,
            &cfg.opencli_args,
            &cfg.working_dir,
            timeout,
            events,
        )),
        Backend::Shell => Box::new(cli::CliBackend::shell(&cfg.working_dir, timeout, events)),
    })
}
