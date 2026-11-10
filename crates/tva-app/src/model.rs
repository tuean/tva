//! 界面数据模型。

use std::time::Duration;

use tva_core::{ActionId, Route};

/// 一次「说话 -> 指令 -> 回复」。
pub struct Turn {
    pub id: u64,
    pub text: String,
    pub route: Route,
    pub reply: String,
    pub actions: Vec<ActionId>,
    pub done: bool,
    pub ok: bool,
}

impl Turn {
    pub fn new(id: u64, text: String, route: Route) -> Self {
        Self {
            id,
            text,
            route,
            reply: String::new(),
            actions: Vec::new(),
            done: false,
            ok: true,
        }
    }
}

/// 动作卡片状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    Running,
    Ok,
    Failed,
}

/// 一个工具调用 / 本机动作。
pub struct ActionCard {
    pub id: ActionId,
    pub command: u64,
    pub name: String,
    pub detail: String,
    pub output: String,
    pub status: ActionStatus,
    pub elapsed: Option<Duration>,
    pub summary: String,
}

/// 动作输出最多保留的字符数，避免长任务把内存和渲染拖垮。
const MAX_OUTPUT_CHARS: usize = 8000;

impl ActionCard {
    pub fn new(id: ActionId, command: u64, name: String, detail: String) -> Self {
        Self {
            id,
            command,
            name,
            detail,
            output: String::new(),
            status: ActionStatus::Running,
            elapsed: None,
            summary: String::new(),
        }
    }

    pub fn push_output(&mut self, chunk: &str) {
        self.output.push_str(chunk);
        if self.output.chars().count() > MAX_OUTPUT_CHARS {
            let keep: String = self
                .output
                .chars()
                .rev()
                .take(MAX_OUTPUT_CHARS)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            self.output = keep;
        }
    }

    /// 只渲染最后 n 行，长输出不至于把界面撑爆。
    pub fn tail_lines(&self, n: usize) -> String {
        let lines: Vec<&str> = self.output.lines().collect();
        if lines.len() <= n {
            return self.output.clone();
        }
        lines[lines.len() - n..].join("\n")
    }
}

/// 时长格式化。
pub fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}
// ───────────────────────── 处理流水线 ─────────────────────────

/// 从按下说话到执行完成，界面上展示的六个阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// 麦克风已开，等待/正在说话。
    Record,
    /// 语音转文字。
    Transcribe,
    /// 解析意图、决定走本地还是 Agent。
    Route,
    /// 大模型思考 / 生成回复。
    Think,
    /// 工具或本机动作执行中。
    Act,
    /// 全部结束。
    Done,
}

impl Phase {
    /// 展示顺序。
    pub const ALL: [Phase; 6] = [
        Phase::Record,
        Phase::Transcribe,
        Phase::Route,
        Phase::Think,
        Phase::Act,
        Phase::Done,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Phase::Record => "录音",
            Phase::Transcribe => "识别",
            Phase::Route => "理解",
            Phase::Think => "思考",
            Phase::Act => "执行",
            Phase::Done => "完成",
        }
    }

}

/// 当前流水线的实时状态。
#[derive(Debug)]
pub struct StepState {
    /// 正在进行的阶段。
    pub phase: Phase,
    /// 该阶段的细节文字，例如"推理中"或工具名。
    pub detail: String,
    /// 当前阶段开始时间。
    pub started: std::time::Instant,
    /// 各阶段累计耗时（会来回跳，比如 思考->执行->思考）。
    pub spent: Vec<(Phase, Duration)>,
    /// 是否有流程在跑（用于决定要不要持续重绘）。
    pub active: bool,
    /// 是否失败结束。
    pub failed: bool,
}

impl Default for StepState {
    fn default() -> Self {
        Self {
            phase: Phase::Record,
            detail: String::new(),
            started: std::time::Instant::now(),
            spent: Vec::new(),
            active: false,
            failed: false,
        }
    }
}

impl StepState {
    /// 开始新一轮：清掉上一轮的耗时记录。
    pub fn reset(&mut self) {
        self.spent.clear();
        self.failed = false;
        self.active = false;
        self.detail.clear();
        self.started = std::time::Instant::now();
    }

    /// 进入某个阶段。同阶段重复调用只更新细节，不重置计时。
    pub fn enter(&mut self, phase: Phase, detail: impl Into<String>) {
        let detail = detail.into();
        if self.active && self.phase == phase {
            self.detail = detail;
            return;
        }
        if self.active {
            self.accumulate();
        }
        self.phase = phase;
        self.detail = detail;
        self.started = std::time::Instant::now();
        self.active = true;
        self.failed = false;
    }

    /// 结束当前流程。
    pub fn finish(&mut self, ok: bool) {
        if self.active {
            self.accumulate();
        }
        self.phase = Phase::Done;
        self.detail = if ok { "已完成".into() } else { "失败".into() };
        self.active = false;
        self.failed = !ok;
    }

    /// 麦克风关闭且没有指令在跑时，收起进度条。
    pub fn idle(&mut self) {
        if matches!(self.phase, Phase::Record | Phase::Transcribe) {
            self.active = false;
            self.detail.clear();
        }
    }

    /// 当前阶段已用时。
    pub fn current_elapsed(&self) -> Duration {
        if self.active { self.started.elapsed() } else { Duration::ZERO }
    }

    /// 某阶段累计耗时。
    pub fn spent_on(&self, phase: Phase) -> Option<Duration> {
        self.spent.iter().rev().find(|(p, _)| *p == phase).map(|(_, d)| *d)
    }

    fn accumulate(&mut self) {
        let elapsed = self.started.elapsed();
        if let Some(entry) = self.spent.iter_mut().find(|(phase, _)| *phase == self.phase) {
            entry.1 += elapsed;
        } else {
            self.spent.push((self.phase, elapsed));
        }
    }
}
