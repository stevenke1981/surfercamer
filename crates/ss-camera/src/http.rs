//! HTTP sources: MJPEG streams and polled JPEG snapshots (basic auth).

use std::time::Duration;

use async_trait::async_trait;
use ss_core::{CameraId, Frame, Result};

use crate::FrameSource;

pub struct MjpegHttpSource {
    _priv: (),
}

impl MjpegHttpSource {
    pub async fn connect(camera_id: CameraId, url: &str, username: Option<&str>, password: Option<&str>) -> Result<Self> {
        let _ = (camera_id, url, username, password);
        todo!()
    }
}

#[async_trait]
impl FrameSource for MjpegHttpSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        todo!()
    }
}

pub struct SnapshotSource {
    _priv: (),
}

impl SnapshotSource {
    pub fn new(camera_id: CameraId, url: &str, username: Option<&str>, password: Option<&str>, interval: Duration) -> Result<Self> {
        let _ = (camera_id, url, username, password, interval);
        todo!()
    }
}

#[async_trait]
impl FrameSource for SnapshotSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        todo!()
    }
}
