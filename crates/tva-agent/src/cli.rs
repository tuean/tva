//! 一次性 CLI 后端：每条指令拉起一个子进程，把输出作为动作日志。
//!
//! 适用于 dsh --profile headless "..."、opencli 子命令，以及直接跑 shell。
//!
//! 注意：dsh 的 headless profile 只在结束时把最终答复写到 stdout，
//! 中间没有工具调用事件，所以步骤提示只能停在"处理中"，到结束时直接跳完成。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tva_core::{ActionId, AgentStage, AppEvent};

use crate::{AgentBackend, EventSink};

/// 看门狗检查间隔。
const WATCH_INTERVAL: Duration = Duration::from_millis(400);

/// 一次性命令行后端。
pub struct CliBackend {
    program: String,
    args: Vec<String>,
    working_dir: PathBuf,
    timeout: Duration,
    events: EventSink,
    /// 当前子进程，读线程和看门狗共享。
    child: Arc<Mutex<Option<Child>>>,
    /// 本次任务是否已经产出过输出（用于把步骤从"处理中"切到"输出中"）。
    produced: Arc<AtomicBool>,
}

impl CliBackend {
    /// program + 固定参数，提交的 prompt 会作为最后一个参数追加。
    pub fn new(
        program: &str,
        args: &[String],
        working_dir: &Path,
        timeout: Duration,
        events: EventSink,
    ) -> Self {
        Self {
            program: program.to_string(),
            args: args.to_vec(),
            working_dir: working_dir.to_path_buf(),
            timeout,
            events,
            child: Arc::new(Mutex::new(None)),
            produced: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 直接把 prompt 当作 shell 命令执行。
    pub fn shell(working_dir: &Path, timeout: Duration, events: EventSink) -> Self {
        Self::new(
            "/bin/zsh",
            &["-lc".to_string()],
            working_dir,
            timeout,
            events,
        )
    }
}

impl AgentBackend for CliBackend {
    fn submit(&mut self, id: u64, prompt: &str) -> Result<()> {
        self.interrupt();
        self.produced.store(false, Ordering::Relaxed);

        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .arg(prompt)
            .current_dir(&self.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 {} 失败（是否已安装并在 PATH 中？）", self.program))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *self.child.lock() = Some(child);

        let action = ActionId(id.wrapping_mul(1000).wrapping_add(1));
        emit(
            &self.events,
            AppEvent::Stage {
                id,
                stage: AgentStage::Started,
            },
        );
        emit(
            &self.events,
            AppEvent::ActionStart {
                id,
                action,
                name: self.program.clone(),
                detail: prompt.to_string(),
            },
        );

        // 读输出。
        {
            let events = self.events.clone();
            let produced = self.produced.clone();
            let program = self.program.clone();
            std::thread::Builder::new()
                .name("tva-cli-output".into())
                .spawn(move || {
                    let started = Instant::now();
                    let mut last = String::new();
                    let mut pump = |reader: Box<dyn BufRead>, events: &EventSink| {
                        for line in reader.lines().map_while(Result::ok) {
                            last = line.clone();
                            if !produced.swap(true, Ordering::Relaxed) {
                                emit(
                                    events,
                                    AppEvent::Stage {
                                        id,
                                        stage: AgentStage::Responding,
                                    },
                                );
                            }
                            emit(
                                events,
                                AppEvent::ActionOutput {
                                    id,
                                    action,
                                    chunk: format!("{line}\n"),
                                },
                            );
                        }
                    };
                    if let Some(stdout) = stdout {
                        pump(Box::new(BufReader::new(stdout)), &events);
                    }
                    if let Some(stderr) = stderr {
                        pump(Box::new(BufReader::new(stderr)), &events);
                    }
                    emit(
                        &events,
                        AppEvent::ActionEnd {
                            id,
                            action,
                            ok: true,
                            summary: if last.is_empty() {
                                format!("{program} 执行结束")
                            } else {
                                last
                            },
                            elapsed: started.elapsed(),
                        },
                    );
                    emit(
                        &events,
                        AppEvent::Stage {
                            id,
                            stage: AgentStage::Settled,
                        },
                    );
                    emit(&events, AppEvent::CommandDone { id, ok: true });
                })?;
        }

        // 看门狗：超时或进程退出后清理。
        {
            let child = self.child.clone();
            let events = self.events.clone();
            let program = self.program.clone();
            let timeout = self.timeout;
            std::thread::Builder::new()
                .name("tva-cli-watchdog".into())
                .spawn(move || {
                    let started = Instant::now();
                    loop {
                        std::thread::sleep(WATCH_INTERVAL);
                        let mut slot = child.lock();
                        let Some(process) = slot.as_mut() else { break };
                        match process.try_wait() {
                            Ok(Some(_)) => {
                                *slot = None;
                                break;
                            }
                            Ok(None) => {
                                if timeout > Duration::ZERO && started.elapsed() > timeout {
                                    log::warn!("{program} 超时（{:?}），已终止", timeout);
                                    let _ = process.kill();
                                    *slot = None;
                                    emit(
                                        &events,
                                        AppEvent::Error {
                                            message: format!("{program} 超时（{}s），已终止", timeout.as_secs()),
                                        },
                                    );
                                    break;
                                }
                            }
                            Err(_) => {
                                *slot = None;
                                break;
                            }
                        }
                    }
                })?;
        }

        Ok(())
    }

    fn interrupt(&mut self) {
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn describe(&self) -> String {
        format!("{} {}", self.program, self.args.join(" "))
            .trim()
            .to_string()
    }
}

impl Drop for CliBackend {
    fn drop(&mut self) {
        self.interrupt();
    }
}

fn emit(events: &EventSink, event: AppEvent) {
    let _ = events.send_blocking(event);
}
