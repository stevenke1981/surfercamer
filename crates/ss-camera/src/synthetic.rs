//! Generated test pattern; lets the whole system run without a camera.

use async_trait::async_trait;
use ss_core::{CameraId, Frame, Result};

use crate::FrameSource;

pub struct SyntheticSource {
    _priv: (),
}

impl SyntheticSource {
    pub fn new(camera_id: CameraId, width: u32, height: u32, fps: u32) -> Self {
        let _ = (camera_id, width, height, fps);
        todo!()
    }

    /// Render frame number `n` as JPEG (deterministic; a box moves across a gradient).
    pub fn render(&self, n: u64) -> Vec<u8> {
        let _ = n;
        todo!()
    }
}

#[async_trait]
impl FrameSource for SyntheticSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        todo!()
    }
}
