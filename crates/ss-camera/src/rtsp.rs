//! RTSP via an `ffmpeg` child process emitting MJPEG on stdout.

use async_trait::async_trait;
use ss_core::{CameraId, Frame, Result};

use crate::FrameSource;

/// True if an `ffmpeg` binary (or `$SS_FFMPEG`) is runnable.
pub fn ffmpeg_available() -> bool {
    todo!()
}

/// ffmpeg arguments used for a stream (exposed for testing). Credentials are
/// injected into the URL userinfo, percent-encoded.
pub fn ffmpeg_args(url: &str, username: Option<&str>, password: Option<&str>, fps: u32) -> Vec<String> {
    let _ = (url, username, password, fps);
    todo!()
}

pub struct RtspSource {
    _priv: (),
}

impl RtspSource {
    pub async fn spawn(camera_id: CameraId, url: &str, username: Option<&str>, password: Option<&str>, fps: u32) -> Result<Self> {
        let _ = (camera_id, url, username, password, fps);
        todo!()
    }
}

#[async_trait]
impl FrameSource for RtspSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        todo!()
    }
}
