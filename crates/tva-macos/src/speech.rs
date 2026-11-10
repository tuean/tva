//! 系统 Speech 框架引擎：AVAudioEngine 采集 + SFSpeechRecognizer 流式识别。
//!
//! ObjC 对象（AVAudioEngine / SFSpeechRecognizer / 请求 / 任务）只存在于工作线程，
//! 跨线程只传递纯 Rust 数据（采样、事件），因此不需要 unsafe impl Send。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use crossbeam_channel::{Receiver, Sender, select};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSError, NSLocale, NSOperationQueue, NSString};
use objc2_speech::{
    SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
    SFSpeechRecognitionTaskHint, SFSpeechRecognizer,
};
use tva_core::{AppEvent, AsrConfig, AsrState};

use crate::Ctrl;
use crate::audio::{AudioInput, VoiceGate};

/// 电平事件的发送间隔。
const LEVEL_INTERVAL: Duration = Duration::from_millis(50);
/// endAudio 之后，识别任务额外存活多久以接收最终结果。
const CLOSE_GRACE: Duration = Duration::from_millis(2500);
/// 回收过期会话的轮询间隔。
const REAP_INTERVAL: Duration = Duration::from_millis(200);

/// 一次聆听会话 = 一个识别请求 + 一个识别任务。
struct Session {
    request: Retained<SFSpeechAudioBufferRecognitionRequest>,
    _task: Retained<SFSpeechRecognitionTask>,
    /// 已经调用过 endAudio()，等待最终结果。
    finishing: bool,
}

/// 诊断当前机器的语音识别可用性。
pub fn diagnose(locale: &str) -> String {
    use objc2_speech::SFSpeechRecognizerAuthorizationStatus;

    let mut out = String::new();
    let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
    let status_text = if status == SFSpeechRecognizerAuthorizationStatus::Authorized {
        "已授权"
    } else if status == SFSpeechRecognizerAuthorizationStatus::Denied {
        "已拒绝"
    } else if status == SFSpeechRecognizerAuthorizationStatus::Restricted {
        "受限"
    } else {
        "未决定"
    };
    out.push_str(&format!("语音识别权限: {status_text}\n"));

    let identifier = NSString::from_str(locale);
    let ns_locale = NSLocale::localeWithLocaleIdentifier(&identifier);
    let recognizer =
        unsafe { SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &ns_locale) };
    match recognizer {
        Some(recognizer) => {
            out.push_str(&format!(
                "识别器({locale}): 服务可用={} 支持设备端离线={}\n",
                unsafe { recognizer.isAvailable() },
                unsafe { recognizer.supportsOnDeviceRecognition() },
            ));
        }
        None => out.push_str(&format!("识别器({locale}): 该语言不受支持\n")),
    }
    out
}

// ───────────────────────────── 工作线程 ─────────────────────────────

pub(crate) fn worker(
    cfg: AsrConfig,
    events: async_channel::Sender<AppEvent>,
    ctrl_tx: Sender<Ctrl>,
    ctrl_rx: Receiver<Ctrl>,
    listening: Arc<AtomicBool>,
) -> Result<()> {
    emit(&events, AppEvent::AsrState(AsrState::Authorizing));

    if !crate::permissions::request_microphone_access(Duration::from_secs(60))
        .context("麦克风权限申请失败")?
    {
        bail!("麦克风权限被拒绝：请在「系统设置 → 隐私与安全性 → 麦克风」中允许 TVA");
    }
    if !crate::permissions::request_speech_access(Duration::from_secs(60))
        .context("语音识别权限申请失败")?
    {
        bail!("语音识别权限被拒绝：请在「系统设置 → 隐私与安全性 → 语音识别」中允许 TVA");
    }

    let recognizer = unsafe {
        let identifier = NSString::from_str(&cfg.locale);
        let locale = NSLocale::localeWithLocaleIdentifier(&identifier);
        SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale)
    }
    .ok_or_else(|| anyhow!("系统不支持识别语言 {}", cfg.locale))?;

    if !unsafe { recognizer.isAvailable() } {
        bail!("语音识别服务当前不可用（检查网络或系统语音识别设置）");
    }

    // 让结果回调跑在专用队列上，不占用 UI 主线程。
    unsafe {
        recognizer.setQueue(&NSOperationQueue::new());
        recognizer.setDefaultTaskHint(SFSpeechRecognitionTaskHint::Dictation);
    }

    let supports_on_device = unsafe { recognizer.supportsOnDeviceRecognition() };
    let on_device = Arc::new(AtomicBool::new(cfg.on_device && supports_on_device));
    if cfg.on_device && !supports_on_device {
        log::warn!("{} 不支持设备端离线识别，将使用在线识别", cfg.locale);
    }

    let input = AudioInput::open()?;
    emit(&events, AppEvent::AsrState(AsrState::Idle));

    let mut session: Option<Session> = None;
    // 已 endAudio、仍在等待最终结果的会话。
    let mut closing: Vec<(Instant, Session)> = Vec::new();
    let mut gate = VoiceGate::new(cfg.silence_rms);
    let mut speech_started: Option<Instant> = None;
    let mut last_voice = Instant::now();
    let mut last_level = Instant::now() - LEVEL_INTERVAL;

    loop {
        select! {
            recv(ctrl_rx) -> msg => {
                let Ok(ctrl) = msg else { break };
                match ctrl {
                    Ctrl::Start => {
                        if !listening.swap(true, Ordering::Relaxed) {
                            log::info!("开始聆听");
                        }
                        if session.is_none() {
                            session = Some(open_session(&recognizer, &events, on_device.clone(), cfg.allow_server_fallback, ctrl_tx.clone())?);
                        }
                        speech_started = None;
                        last_voice = Instant::now();
                        if let Err(message) = input.start() {
                            emit(&events, AppEvent::AsrError { message: format!("音频引擎启动失败: {message}") });
                            listening.store(false, Ordering::Relaxed);
                            continue;
                        }
                        emit(&events, AppEvent::AsrState(AsrState::Listening));
                    }
                    Ctrl::Stop => {
                        listening.store(false, Ordering::Relaxed);
                        // endAudio 之后识别器还会吐一次最终结果，任务必须多活一会儿。
                        if let Some(session) = session.take() {
                            unsafe { session.request.endAudio() };
                            closing.push((Instant::now() + CLOSE_GRACE, session));
                        }
                        input.stop();
                        speech_started = None;
                        emit(&events, AppEvent::AsrState(AsrState::Idle));
                        log::info!("停止聆听");
                    }
                    Ctrl::Restart => {
                        if listening.load(Ordering::Relaxed) {
                            if let Some(old) = session.take() {
                                unsafe { old.request.endAudio() };
                                closing.push((Instant::now() + CLOSE_GRACE, old));
                            }
                            session = Some(open_session(&recognizer, &events, on_device.clone(), cfg.allow_server_fallback, ctrl_tx.clone())?);
                        }
                    }
                    Ctrl::Fallback => {
                        if on_device.swap(false, Ordering::Relaxed) {
                            log::warn!("设备端离线识别不可用，回退到在线识别");
                            emit(&events, AppEvent::Error {
                                message: "系统「Siri 与听写」未开启，离线识别不可用，已自动切换到在线识别（音频会发送到 Apple 服务器）。".into(),
                            });
                        }
                        if listening.load(Ordering::Relaxed) {
                            if let Some(old) = session.take() {
                                unsafe { old.request.endAudio() };
                                closing.push((Instant::now() + CLOSE_GRACE, old));
                            }
                            session = Some(open_session(&recognizer, &events, on_device.clone(), cfg.allow_server_fallback, ctrl_tx.clone())?);
                        }
                    }
                    Ctrl::Fatal => {
                        listening.store(false, Ordering::Relaxed);
                        if let Some(old) = session.take() {
                            unsafe { old.request.endAudio() };
                        }
                        input.stop();
                        emit(&events, AppEvent::AsrState(AsrState::Failed));
                    }
                    Ctrl::Shutdown => break,
                }
            }

            recv(input.receiver()) -> chunk => {
                let Ok(chunk) = chunk else { break };
                if !listening.load(Ordering::Relaxed) {
                    continue;
                }
                let now = Instant::now();
                if now.duration_since(last_level) >= LEVEL_INTERVAL {
                    last_level = now;
                    emit(&events, AppEvent::Level { rms: chunk.rms });
                }

                let Some(active) = session.as_mut() else { continue };
                if active.finishing {
                    continue;
                }

                if gate.update(chunk.rms) {
                    last_voice = now;
                    if speech_started.is_none() {
                        speech_started = Some(now);
                        emit(&events, AppEvent::AsrState(AsrState::Recognizing));
                    }
                }

                if let Some(buffer) = input.to_pcm_buffer(&chunk) {
                    unsafe { active.request.appendAudioPCMBuffer(&buffer) };
                }

                let spoke = speech_started
                    .map(|t| now.duration_since(t) >= Duration::from_millis(cfg.min_speech_ms))
                    .unwrap_or(false);
                if spoke && now.duration_since(last_voice) >= Duration::from_millis(cfg.silence_ms) {
                    active.finishing = true;
                    unsafe { active.request.endAudio() };
                }
            }

            default(REAP_INTERVAL) => {
                let now = Instant::now();
                closing.retain(|(deadline, _)| *deadline > now);
            }
        }
    }

    if let Some(session) = session.take() {
        unsafe { session.request.endAudio() };
    }
    drop(closing);
    input.stop();
    emit(&events, AppEvent::AsrState(AsrState::Idle));
    Ok(())
}

/// 创建一个新的识别请求 + 任务。
fn open_session(
    recognizer: &SFSpeechRecognizer,
    events: &async_channel::Sender<AppEvent>,
    on_device: Arc<AtomicBool>,
    allow_fallback: bool,
    ctrl: Sender<Ctrl>,
) -> Result<Session> {
    let request = unsafe { SFSpeechAudioBufferRecognitionRequest::new() };
    unsafe {
        request.setShouldReportPartialResults(true);
        request.setAddsPunctuation(true);
        request.setRequiresOnDeviceRecognition(on_device.load(Ordering::Relaxed));
    }

    let events = events.clone();
    let handler =
        RcBlock::new(move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
            if !error.is_null() {
                let error = unsafe { &*error };
                let code = error.code();
                let domain = error.domain().to_string();
                let message = error.localizedDescription().to_string();

                // 正常的断句/无声结束，直接开下一段。
                let no_speech = domain.contains("kAFAssistantErrorDomain")
                    && matches!(code, 203 | 216 | 1101 | 1110);
                // 系统「Siri 与听写」关闭 -> 设备端离线识别不可用。
                let dictation_disabled = domain.contains("kLSRErrorDomain") && code == 201;

                if no_speech {
                    log::debug!("识别结束（正常）: {message}");
                    let _ = ctrl.send(Ctrl::Restart);
                } else if dictation_disabled && on_device.load(Ordering::Relaxed) {
                    if allow_fallback {
                        let _ = ctrl.send(Ctrl::Fallback);
                    } else {
                        let _ = events.send_blocking(AppEvent::AsrError {
                            message: format!("{message}（可在配置中开启 allow_server_fallback 使用在线识别）"),
                        });
                        let _ = ctrl.send(Ctrl::Fatal);
                    }
                } else {
                    log::warn!("识别错误: {domain}/{code} {message}");
                    let _ = events.send_blocking(AppEvent::AsrError { message });
                    let _ = ctrl.send(Ctrl::Fatal);
                }
                return;
            }
            if result.is_null() {
                return;
            }
            let result = unsafe { &*result };
            let text = unsafe { result.bestTranscription().formattedString() }.to_string();
            if unsafe { result.isFinal() } {
                let _ = events.send_blocking(AppEvent::Final { text });
                let _ = ctrl.send(Ctrl::Restart);
            } else if !text.is_empty() {
                let _ = events.send_blocking(AppEvent::Partial { text });
            }
        });

    let task = unsafe { recognizer.recognitionTaskWithRequest_resultHandler(&request, &handler) };
    Ok(Session {
        request,
        _task: task,
        finishing: false,
    })
}

fn emit(events: &async_channel::Sender<AppEvent>, event: AppEvent) {
    let _ = events.send_blocking(event);
}
