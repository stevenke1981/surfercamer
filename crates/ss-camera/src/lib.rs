//! Camera frame sources. Every source yields JPEG [`Frame`]s. See docs/SPEC.md §6.

pub mod http;
pub mod jpeg;
pub mod mjpeg;
pub mod onvif;
pub mod rtsp;
pub mod synthetic;

use async_trait::async_trait;
use ss_core::{CameraId, CameraSource, Frame, Result};

/// A connected stream of frames. `next_frame` returns `Error::Camera` when
/// the stream breaks; the engine then drops the source and reconnects.
#[async_trait]
pub trait FrameSource: Send {
    async fn next_frame(&mut self) -> Result<Frame>;
}

/// Build and connect the right source for a camera configuration.
pub async fn connect(camera_id: CameraId, source: &CameraSource) -> Result<Box<dyn FrameSource>> {
    let _ = (camera_id, source);
    todo!()
}
