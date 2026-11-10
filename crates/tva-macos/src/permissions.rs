//! 麦克风与语音识别权限申请（阻塞式，带超时）。

use std::time::Duration;

use anyhow::{Result, bail};
use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_speech::{SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus};

/// 申请麦克风权限，返回是否已授权。
pub fn request_microphone_access(timeout: Duration) -> Result<bool> {
    let media =
        unsafe { AVMediaTypeAudio }.ok_or_else(|| anyhow::anyhow!("AVMediaTypeAudio 不可用"))?;
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
    if status == AVAuthorizationStatus::Authorized {
        return Ok(true);
    }
    if status == AVAuthorizationStatus::Denied || status == AVAuthorizationStatus::Restricted {
        return Ok(false);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler);
    }
    match rx.recv_timeout(timeout) {
        Ok(granted) => Ok(granted),
        Err(_) => bail!("申请麦克风权限超时"),
    }
}

/// 申请语音识别权限，返回是否已授权。
pub fn request_speech_access(timeout: Duration) -> Result<bool> {
    let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
    if status == SFSpeechRecognizerAuthorizationStatus::Authorized {
        return Ok(true);
    }
    if status == SFSpeechRecognizerAuthorizationStatus::Denied
        || status == SFSpeechRecognizerAuthorizationStatus::Restricted
    {
        return Ok(false);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let handler = RcBlock::new(move |status: SFSpeechRecognizerAuthorizationStatus| {
        let _ = tx.send(status == SFSpeechRecognizerAuthorizationStatus::Authorized);
    });
    unsafe {
        SFSpeechRecognizer::requestAuthorization(&handler);
    }
    match rx.recv_timeout(timeout) {
        Ok(granted) => Ok(granted),
        Err(_) => bail!("申请语音识别权限超时"),
    }
}
