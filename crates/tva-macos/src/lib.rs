//! macOS 平台层。
//!
//! 两个可切换的语音识别引擎，共用同一套麦克风采集（audio 模块）和句柄接口：
//!
//! - `speech`：系统 Speech 框架（零模型、流式 partial，需要「Siri 与听写」或联网）
//! - `whisper`：本地 whisper.cpp（完全离线，模型自选，CPU 推理）

#![cfg(target_os = "macos")]

pub mod audio;
pub mod permissions;
pub mod speech;
pub mod whisper;

pub use speech::diagnose;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{Sender, unbounded};
use tva_core::{AppEvent, AsrConfig, AsrEngine};

/// 识别线程的控制指令（两个引擎共用，各自只处理自己关心的）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum Ctrl {
    /// 开始聆听会话。
    Start,
    /// 结束聆听并关闭麦克风。
    Stop,
    /// 断句后立刻开新的一段（系统 Speech 引擎用）。
    Restart,
    /// 设备端离线识别不可用，回退在线（系统 Speech 引擎用）。
    Fallback,
    /// 致命错误，停止聆听并上报。
    Fatal,
    /// 退出线程。
    Shutdown,
}

/// 识别器句柄：可以从任意线程安全地 start/stop。
pub struct RecognizerHandle {
    ctrl: Sender<Ctrl>,
    listening: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

/// 语义化别名。
pub type Recognizer = RecognizerHandle;

impl RecognizerHandle {
    /// 按配置启动对应的识别线程（不打开麦克风，等待 start()）。
    pub fn spawn(cfg: AsrConfig, events: async_channel::Sender<AppEvent>) -> Self {
        Self::spawn_with_flag(cfg, events, Arc::new(AtomicBool::new(false)))
    }

    /// 同上，但复用外部传入的"是否在聆听"标志，方便运行时换引擎。
    pub fn spawn_with_flag(
        cfg: AsrConfig,
        events: async_channel::Sender<AppEvent>,
        listening: Arc<AtomicBool>,
    ) -> Self {
        let (ctrl_tx, ctrl_rx) = unbounded();
        let listening_worker = listening.clone();
        let ctrl_worker = ctrl_tx.clone();
        let engine = cfg.engine;
        let join = std::thread::Builder::new()
            .name("tva-asr".into())
            .spawn(move || {
                log::info!("语音识别引擎: {engine:?}");
                let result = match engine {
                    AsrEngine::Macos => {
                        speech::worker(cfg, events.clone(), ctrl_worker, ctrl_rx, listening_worker)
                    }
                    AsrEngine::Whisper => {
                        whisper::worker(cfg, events.clone(), ctrl_worker, ctrl_rx, listening_worker)
                    }
                };
                if let Err(err) = result {
                    log::error!("识别线程退出: {err:#}");
                    let _ = events.send_blocking(AppEvent::AsrError {
                        message: format!("{err:#}"),
                    });
                }
            })
            .expect("无法创建识别线程");
        Self {
            ctrl: ctrl_tx,
            listening,
            join: Some(join),
        }
    }

    /// 是否正在聆听。
    pub fn is_listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }

    /// 开始聆听（幂等）。
    pub fn start(&self) {
        let _ = self.ctrl.send(Ctrl::Start);
    }

    /// 停止聆听并关闭麦克风（幂等）。
    pub fn stop(&self) {
        let _ = self.ctrl.send(Ctrl::Stop);
    }

    /// 开/关切换，返回切换后的状态。
    pub fn toggle(&self) -> bool {
        if self.is_listening() {
            self.stop();
            false
        } else {
            self.start();
            true
        }
    }
}

impl Drop for RecognizerHandle {
    fn drop(&mut self) {
        let _ = self.ctrl.send(Ctrl::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 可运行时换引擎的识别器管理器。
///
/// 界面上的"引擎切换"按钮调用 switch()：先让旧引擎线程退出，
/// 再用新配置起一个，如果切换前正在聆听就自动继续聆听。
pub struct AsrManager {
    events: async_channel::Sender<AppEvent>,
    inner: parking_lot::Mutex<Option<RecognizerHandle>>,
    cfg: parking_lot::Mutex<AsrConfig>,
    listening: Arc<AtomicBool>,
}

impl AsrManager {
    pub fn new(cfg: AsrConfig, events: async_channel::Sender<AppEvent>) -> Self {
        let listening = Arc::new(AtomicBool::new(false));
        let handle =
            RecognizerHandle::spawn_with_flag(cfg.clone(), events.clone(), listening.clone());
        Self {
            events,
            inner: parking_lot::Mutex::new(Some(handle)),
            cfg: parking_lot::Mutex::new(cfg),
            listening,
        }
    }

    /// 当前引擎配置。
    pub fn config(&self) -> AsrConfig {
        self.cfg.lock().clone()
    }

    /// 换一个引擎/模型。返回后新引擎已经就绪。
    ///
    /// 先把旧线程彻底停掉再起新的：否则两套引擎会短暂并存，
    /// 旧引擎退出时发出的 Idle 事件会把界面状态带偏。
    pub fn switch(&self, cfg: AsrConfig) -> anyhow::Result<()> {
        let was_listening = self.listening.load(Ordering::Relaxed);

        // 1. 停旧引擎（stop 让麦克风立刻关掉）。
        if let Some(handle) = self.inner.lock().as_ref() {
            handle.stop();
        }
        // 2. 取走并等待旧线程退出。
        let old = self.inner.lock().take();
        drop(old);

        // 3. 起新引擎。
        let handle =
            RecognizerHandle::spawn_with_flag(cfg.clone(), self.events.clone(), self.listening.clone());
        *self.inner.lock() = Some(handle);
        *self.cfg.lock() = cfg;

        if was_listening
            && let Some(handle) = self.inner.lock().as_ref()
        {
            handle.start();
        }
        Ok(())
    }

    pub fn is_listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }

    pub fn start(&self) {
        if let Some(handle) = self.inner.lock().as_ref() {
            handle.start();
        }
    }

    pub fn stop(&self) {
        if let Some(handle) = self.inner.lock().as_ref() {
            handle.stop();
        }
    }

    pub fn toggle(&self) -> bool {
        if self.is_listening() {
            self.stop();
            false
        } else {
            self.start();
            true
        }
    }
}
