//! TVA 核心：跨线程事件、语音/执行状态机与配置。

pub mod config;
pub mod event;

pub use config::{
    AgentConfig, AsrConfig, AsrEngine, AsrPreset, Backend, Config, HotkeyConfig, UiConfig,
    DEFAULT_SYSTEM_PROMPT, default_model_path, default_partial_model_path, models_dir,
};
pub use event::{ActionId, AgentStage, AppEvent, AsrState, EngineState, Route};

/// 全局指令序号生成器（单调递增）。
#[derive(Debug, Default)]
pub struct IdGen(std::sync::atomic::AtomicU64);

impl IdGen {
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }
}
