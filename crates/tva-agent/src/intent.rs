//! 轻量本地意图路由。
//!
//! 目标不是做 NLU，而是把「打开 XX」「执行 XX」这类低延迟、确定性高的口语指令
//! 直接在本地执行；其余全部交给 Agent，让 Agent 自己规划并调用工具。

/// 路由结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 本地直接执行。
    Local(LocalAction),
    /// 交给 Agent 后端。
    Agent,
}

/// 可以在本地直接完成的最小动作集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAction {
    /// 打开 App 或文件/URL。
    Open(String),
    /// 用默认浏览器搜索。
    Search(String),
    /// 在 shell 中执行。
    Shell(String),
    /// 截屏到桌面。
    Screenshot,
}

const OPEN_PREFIXES: &[&str] = &["打开", "启动", "open ", "launch "];
const SEARCH_PREFIXES: &[&str] = &["搜索", "搜一下", "百度", "谷歌", "search ", "google "];
const SHELL_PREFIXES: &[&str] = &["执行命令", "执行", "运行命令", "运行", "shell ", "run "];
const SCREENSHOT: &[&str] = &["截图", "截个屏", "screenshot", "screen shot"];

/// 判断一段文本应该走哪条通道。
pub fn decide(text: &str) -> Decision {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    // 明确指定走 Agent 的说法。
    if lower.starts_with("agent") || trimmed.starts_with("助手") || trimmed.starts_with("问一下") {
        return Decision::Agent;
    }

    if SCREENSHOT.iter().any(|k| lower == k.to_lowercase()) {
        return Decision::Local(LocalAction::Screenshot);
    }

    // 显式 shell 前缀优先于「打开」。
    for prefix in SHELL_PREFIXES {
        if let Some(rest) = strip_prefix(trimmed, prefix)
            && !rest.is_empty()
        {
            return Decision::Local(LocalAction::Shell(rest.to_string()));
        }
    }

    for prefix in SEARCH_PREFIXES {
        if let Some(rest) = strip_prefix(trimmed, prefix)
            && !rest.is_empty()
        {
            return Decision::Local(LocalAction::Search(rest.to_string()));
        }
    }

    for prefix in OPEN_PREFIXES {
        if let Some(rest) = strip_prefix(trimmed, prefix)
            && !rest.is_empty()
        {
            return Decision::Local(LocalAction::Open(rest.to_string()));
        }
    }

    Decision::Agent
}

fn strip_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let lower = text.to_lowercase();
    if lower.starts_with(&prefix.to_lowercase()) {
        Some(text[prefix.len().min(text.len())..].trim_start_matches([' ', '：', ':', '，', ',']))
    } else {
        None
    }
}

impl LocalAction {
    /// 执行本地动作，把过程以 AppEvent 的形式发出去。
    pub fn execute(&self, id: u64, events: async_channel::Sender<tva_core::AppEvent>) {
        use std::process::{Command, Stdio};

        use tva_core::{ActionId, AgentStage, AppEvent};

        let action_id = ActionId(id.wrapping_mul(1000));
        let (name, detail, program, args): (&str, String, String, Vec<String>) = match self {
            LocalAction::Open(target) => {
                ("open", target.clone(), "open".into(), vec![target.clone()])
            }
            LocalAction::Search(query) => {
                let url = format!("https://www.bing.com/search?q={}", urlencoding(query));
                ("search", url.clone(), "open".into(), vec![url])
            }
            LocalAction::Shell(cmd) => (
                "shell",
                cmd.clone(),
                "/bin/zsh".into(),
                vec!["-lc".into(), cmd.clone()],
            ),
            LocalAction::Screenshot => {
                let path = screenshot_path();
                (
                    "screencapture",
                    path.clone(),
                    "screencapture".into(),
                    vec!["-x".into(), path],
                )
            }
        };

        let _ = events.send_blocking(AppEvent::Stage {
            id,
            stage: AgentStage::Tool {
                name: name.to_string(),
            },
        });
        let _ = events.send_blocking(AppEvent::ActionStart {
            id,
            action: action_id,
            name: name.to_string(),
            detail: detail.clone(),
        });

        let started = std::time::Instant::now();
        let output = Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .output();

        let (ok, summary) = match output {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if !text.is_empty() {
                    let _ = events.send_blocking(AppEvent::ActionOutput {
                        id,
                        action: action_id,
                        chunk: text.clone(),
                    });
                }
                if !err.is_empty() {
                    text = if text.is_empty() {
                        err
                    } else {
                        format!("{text}\n{err}")
                    };
                }
                let ok = output.status.success();
                let summary = if ok {
                    if detail.is_empty() {
                        "完成".into()
                    } else {
                        format!("完成: {detail}")
                    }
                } else {
                    format!("失败: {text}")
                };
                (ok, summary)
            }
            Err(err) => (false, format!("无法执行 {program}: {err}")),
        };

        let _ = events.send_blocking(AppEvent::ActionEnd {
            id,
            action: action_id,
            ok,
            summary,
            elapsed: started.elapsed(),
        });
        let _ = events.send_blocking(AppEvent::Stage {
            id,
            stage: AgentStage::Settled,
        });
        let _ = events.send_blocking(AppEvent::CommandDone { id, ok });
    }
}

fn screenshot_path() -> String {
    let dir = dirs::desktop_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("tva-screenshot-{stamp}.png"))
        .to_string_lossy()
        .into_owned()
}

/// 极简 percent-encoding，够用即可。
fn urlencoding(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_app_goes_local() {
        assert_eq!(
            decide("打开 计算器"),
            Decision::Local(LocalAction::Open("计算器".into()))
        );
        assert_eq!(
            decide("open Calculator"),
            Decision::Local(LocalAction::Open("Calculator".into()))
        );
    }

    #[test]
    fn shell_prefix_wins_over_open() {
        assert_eq!(
            decide("执行 open -a Calculator"),
            Decision::Local(LocalAction::Shell("open -a Calculator".into()))
        );
    }

    #[test]
    fn search_uses_browser() {
        assert_eq!(
            decide("搜索 rust gpui"),
            Decision::Local(LocalAction::Search("rust gpui".into()))
        );
    }

    #[test]
    fn screenshot_keyword() {
        assert_eq!(decide("截图"), Decision::Local(LocalAction::Screenshot));
        assert_eq!(decide("Screenshot"), Decision::Local(LocalAction::Screenshot));
    }

    #[test]
    fn everything_else_goes_to_agent() {
        assert_eq!(decide("把桌面的截图整理到 Downloads 里"), Decision::Agent);
        assert_eq!(decide("帮我看看这个项目为什么编译不过"), Decision::Agent);
        assert_eq!(decide("助手 打开计算器"), Decision::Agent);
    }

    #[test]
    fn bare_prefix_is_not_a_command() {
        assert_eq!(decide("打开"), Decision::Agent);
        assert_eq!(decide("搜索"), Decision::Agent);
    }

    #[test]
    fn urlencoding_escapes_spaces_and_chinese() {
        assert_eq!(urlencoding("a b"), "a%20b");
        assert!(urlencoding("中文").starts_with('%'));
    }
}
