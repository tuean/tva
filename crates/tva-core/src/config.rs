//! 配置文件：macOS 下为 ~/Library/Application Support/tva/config.toml
//! （即 dirs::config_dir()/tva/config.toml），首次运行会自动生成。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub asr: AsrConfig,
    pub agent: AgentConfig,
    pub hotkey: HotkeyConfig,
    pub ui: UiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            asr: AsrConfig::default(),
            agent: AgentConfig::default(),
            hotkey: HotkeyConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

/// 语音识别配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    /// 识别引擎：`macos`（系统 Speech 框架）或 `whisper`（本地 whisper.cpp）。
    pub engine: AsrEngine,
    /// BCP-47 语言标识，例如 `zh-CN` / `en-US`。
    pub locale: String,
    /// 优先使用设备端离线识别（更私密、更快）。若系统「Siri 与听写」未开启，
    /// 会自动回退到在线识别（见 allow_server_fallback）。
    pub on_device: bool,
    /// 离线识别不可用时，是否允许回退到 Apple 在线识别。
    pub allow_server_fallback: bool,
    /// 一段发言结束后的静音判定时长（毫秒），用于切分最终结果。
    pub silence_ms: u64,
    /// 电平低于该阈值视为静音。
    pub silence_rms: f32,
    /// 最短发言时长（毫秒），过短的片段丢弃。
    pub min_speech_ms: u64,

    // ── 以下仅 whisper 引擎使用 ──
    /// 最终结果用的模型（ggml-*.bin），越大约准。
    pub model_path: PathBuf,
    /// 实时预览用的快速模型。留空 = 和 model_path 同一个。
    /// whisper 的解码耗时几乎与音频长度无关（固定 30s 窗口），
    /// 所以 tiny（~150ms）完全够做预览，最终结果再交给大模型。
    pub partial_model_path: PathBuf,
    /// 推理线程数，0 表示按 CPU 核心数自动。
    pub whisper_threads: i32,
    /// 说话过程中每隔多久重新解码一次，用于"实时"预览（毫秒）。
    pub partial_interval_ms: u64,
    /// 初始提示词，能显著改善中文标点和用词。
    pub whisper_prompt: String,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            engine: AsrEngine::Macos,
            locale: "zh-CN".into(),
            on_device: true,
            allow_server_fallback: true,
            silence_ms: 1200,
            silence_rms: 0.012,
            min_speech_ms: 300,
            model_path: default_model_path(),
            partial_model_path: default_partial_model_path(),
            whisper_threads: 0,
            partial_interval_ms: 1200,
            whisper_prompt: "以下是普通话的句子。".into(),
        }
    }
}

/// 模型目录：~/Library/Application Support/tva/models
pub fn models_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tva")
        .join("models")
}

/// 默认模型位置：<models>/ggml-small.bin
pub fn default_model_path() -> PathBuf {
    models_dir().join("ggml-small.bin")
}

/// 默认预览模型：<models>/ggml-tiny.bin（不存在时引擎会自动回退到 model_path）
pub fn default_partial_model_path() -> PathBuf {
    models_dir().join("ggml-tiny.bin")
}

/// 语音识别预设：界面上可以一键切换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrPreset {
    /// 系统 Speech 框架：最实时，但需要「Siri 与听写」或联网。
    MacosSpeech,
    /// whisper 极速：tiny 模型，预览和最终都用它，CPU 占用最低。
    WhisperTiny,
    /// whisper 标准：base 出最终结果，tiny 出预览。
    WhisperBase,
    /// whisper 精准：small 出最终结果，tiny 出预览。
    WhisperSmall,
}

impl AsrPreset {
    /// 界面上的展示顺序。
    pub const ALL: &'static [AsrPreset] = &[
        AsrPreset::MacosSpeech,
        AsrPreset::WhisperTiny,
        AsrPreset::WhisperBase,
        AsrPreset::WhisperSmall,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            AsrPreset::MacosSpeech => "系统 Speech",
            AsrPreset::WhisperTiny => "whisper 极速",
            AsrPreset::WhisperBase => "whisper 标准",
            AsrPreset::WhisperSmall => "whisper 精准",
        }
    }

    /// 悬停/底部提示用的说明。
    pub fn hint(&self) -> &'static str {
        match self {
            AsrPreset::MacosSpeech => "最实时，离线不可用时音频会发到 Apple",
            AsrPreset::WhisperTiny => "完全离线，预览最快，识别错字较多",
            AsrPreset::WhisperBase => "完全离线，速度与准确度折中",
            AsrPreset::WhisperSmall => "完全离线，最准，预览用 tiny 所以依旧跟手",
        }
    }

    /// 最终结果用的模型文件名。
    pub fn model_file(&self) -> Option<&'static str> {
        match self {
            AsrPreset::MacosSpeech => None,
            AsrPreset::WhisperTiny => Some("ggml-tiny.bin"),
            AsrPreset::WhisperBase => Some("ggml-base.bin"),
            AsrPreset::WhisperSmall => Some("ggml-small.bin"),
        }
    }

    /// 预览用的模型文件名（None = 与最终模型相同）。
    pub fn partial_model_file(&self) -> Option<&'static str> {
        match self {
            AsrPreset::MacosSpeech | AsrPreset::WhisperTiny => None,
            _ => Some("ggml-tiny.bin"),
        }
    }

    /// 把预设写进配置。
    pub fn apply(&self, cfg: &mut AsrConfig) {
        match self {
            AsrPreset::MacosSpeech => {
                cfg.engine = AsrEngine::Macos;
            }
            _ => {
                cfg.engine = AsrEngine::Whisper;
                let dir = models_dir();
                if let Some(model) = self.model_file() {
                    cfg.model_path = dir.join(model);
                }
                cfg.partial_model_path = match self.partial_model_file() {
                    Some(file) if dir.join(file).exists() => dir.join(file),
                    _ => PathBuf::new(),
                };
            }
        }
    }

    /// 判断当前配置属于哪个预设。
    pub fn matches(&self, cfg: &AsrConfig) -> bool {
        match self {
            AsrPreset::MacosSpeech => cfg.engine == AsrEngine::Macos,
            _ => {
                if cfg.engine != AsrEngine::Whisper {
                    return false;
                }
                let Some(model) = self.model_file() else {
                    return false;
                };
                if cfg.model_path.file_name().and_then(|n| n.to_str()) != Some(model) {
                    return false;
                }
                let expected = self.partial_model_file().unwrap_or(model);
                let actual = cfg
                    .partial_model_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(model);
                actual == expected
            }
        }
    }

    /// 该预设需要的模型文件，用于检查是否已下载。
    pub fn required_models(&self) -> Vec<PathBuf> {
        let dir = models_dir();
        let mut out = Vec::new();
        if let Some(model) = self.model_file() {
            out.push(dir.join(model));
        }
        if let Some(model) = self.partial_model_file() {
            out.push(dir.join(model));
        }
        out
    }

    /// 模型是否齐备。
    pub fn ready(&self) -> bool {
        self.required_models()
            .iter()
            .all(|path| path.exists() && std::fs::metadata(path).map(|m| m.len() > 10 * 1024 * 1024).unwrap_or(false))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AsrEngine {
    /// 系统 Speech 框架。
    Macos,
    /// 本地 whisper.cpp（whisper-rs）。
    Whisper,
}

impl AsrEngine {
    pub fn label(&self) -> &'static str {
        match self {
            AsrEngine::Macos => "macOS Speech",
            AsrEngine::Whisper => "whisper.cpp (本地)",
        }
    }
}

/// Agent 后端配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// 后端：`pi` | `dsh` | `opencli` | `shell`
    pub backend: Backend,
    /// pi 可执行文件与参数（RPC 模式）。
    pub pi_command: String,
    pub pi_args: Vec<String>,
    /// dsh 可执行文件与参数（headless 一次性任务）。
    pub dsh_command: String,
    pub dsh_args: Vec<String>,
    /// opencli 可执行文件与参数。
    pub opencli_command: String,
    pub opencli_args: Vec<String>,
    /// Agent 工作目录。
    pub working_dir: PathBuf,
    /// 单条指令超时（秒），0 表示不限制。
    pub timeout_secs: u64,
    /// 系统提示词，用于把口语化的语音整理成明确的本机操作指令。
    pub system_prompt: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Pi,
            pi_command: "pi".into(),
            pi_args: vec!["--mode".into(), "rpc".into(), "--no-session".into()],
            dsh_command: "dsh".into(),
            dsh_args: vec!["--profile".into(), "headless".into()],
            opencli_command: "opencli".into(),
            opencli_args: vec![],
            working_dir: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            timeout_secs: 300,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
        }
    }
}

pub const DEFAULT_SYSTEM_PROMPT: &str = "你是一个 macOS 本机语音助手。用户通过语音下达指令，请把它当成明确的操作意图，直接使用工具在本机完成任务，然后用一两句话中文汇报结果。不要反问，不要输出多余解释；若确实无法完成，简短说明原因。";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Pi,
    Dsh,
    Opencli,
    Shell,
}

/// 全局快捷键配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// 按住说话（push-to-talk），例如 `Alt+Space`。
    pub push_to_talk: String,
    /// 单击开/关录音。
    pub toggle: String,
    /// 是否注册全局快捷键（关闭后只能用界面按钮）。
    pub enabled: bool,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            push_to_talk: "Alt+Space".into(),
            toggle: "Alt+Enter".into(),
            enabled: true,
        }
    }
}

/// 界面配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// 窗口置顶。
    pub always_on_top: bool,
    /// 用 `say` 朗读 Agent 的最终回复。
    pub speak_replies: bool,
    /// 执行本地 shell 动作前需要确认。
    pub confirm_shell: bool,
    /// 最多保留的动作卡片数。
    pub max_actions: usize,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            always_on_top: true,
            speak_replies: false,
            confirm_shell: false,
            max_actions: 200,
        }
    }
}

impl Config {
    /// 配置文件路径。
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tva")
            .join("config.toml")
    }

    /// 写回配置文件（界面切换引擎时用）。
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// 读取配置；文件不存在时返回默认值并写出模板。
    pub fn load() -> anyhow::Result<Self> {
        let path = Self::path();
        if !path.exists() {
            let cfg = Self::default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, toml::to_string_pretty(&cfg)?).ok();
            log::info!("已生成默认配置: {}", path.display());
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(&path)?;
        let cfg: Self = toml::from_str(&raw)?;
        log::info!("已加载配置: {}", path.display());
        Ok(cfg)
    }
}
