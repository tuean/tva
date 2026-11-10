//! 从音频/识别/Agent 各线程流向 UI 的统一事件。

use std::time::Duration;

/// 语音识别子系统的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrState {
    /// 未启动
    Idle,
    /// 正在申请权限
    Authorizing,
    /// 麦克风已打开，等待说话
    Listening,
    /// 正在识别（已有音频流入）
    Recognizing,
    /// 出错（附错误信息，走 AppEvent::AsrError）
    Failed,
}

/// 界面顶部展示的整体状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Listening,
    Recognizing,
    Thinking,
    Acting,
    Error,
}

impl EngineState {
    pub fn label(&self) -> &'static str {
        match self {
            EngineState::Idle => "待命",
            EngineState::Listening => "聆听中",
            EngineState::Recognizing => "识别中",
            EngineState::Thinking => "思考中",
            EngineState::Acting => "执行中",
            EngineState::Error => "异常",
        }
    }
}

/// 一条动作（工具调用）在 UI 上的唯一标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionId(pub u64);

/// Agent 内部的细粒度阶段，用于界面上的步骤提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStage {
    /// 指令已提交，等模型响应。
    Submitted,
    /// 模型开始处理这一轮。
    Started,
    /// 模型正在推理（thinking 增量）。
    Reasoning,
    /// 模型正在生成回复文字。
    Responding,
    /// 正在执行某个工具 / 本机动作。
    Tool { name: String },
    /// 工具结束，回到模型继续。
    ToolDone { name: String },
    /// 整轮结束（不会再有自动续跑）。
    Settled,
}

/// 指令被路由到了哪条通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// 直接交给 Agent（pi / dsh / opencli）
    Agent,
    /// 本地意图（打开 App、执行 shell 等）
    Local,
}

impl Route {
    pub fn label(&self) -> &'static str {
        match self {
            Route::Agent => "AGENT",
            Route::Local => "LOCAL",
        }
    }
}

/// 事件：任意线程 -> UI。
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// 麦克风电平（0.0..=1.0），用于波形显示。
    Level { rms: f32 },

    /// 识别状态变化。
    AsrState(AsrState),

    /// 实时部分转写结果（覆盖显示）。
    Partial { text: String },

    /// 一次发言的最终转写结果。
    Final { text: String },

    /// 语音链路错误。
    AsrError { message: String },

    /// 一条指令被接收，开始执行。
    Command { id: u64, text: String, route: Route },

    /// Agent 内部阶段变化（用于"当前进行到哪一步"）。
    Stage { id: u64, stage: AgentStage },

    /// Agent 回复的增量文本。
    AgentDelta { id: u64, text: String },

    /// Agent 的完整回复（覆盖增量）。
    AgentText { id: u64, text: String },

    /// 动作开始。
    ActionStart {
        id: u64,
        action: ActionId,
        name: String,
        detail: String,
    },

    /// 动作输出增量。
    ActionOutput {
        id: u64,
        action: ActionId,
        chunk: String,
    },

    /// 动作结束。
    ActionEnd {
        id: u64,
        action: ActionId,
        ok: bool,
        summary: String,
        elapsed: Duration,
    },

    /// 危险动作需要用户确认后才执行。
    ConfirmRequest { id: u64, text: String, detail: String },

    /// 指令执行结束。
    CommandDone { id: u64, ok: bool },

    /// 通用错误。
    Error { message: String },
}
