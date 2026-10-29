//! Camera frame sources. Every source yields JPEG [`Frame`]s. See docs/SPEC.md §6.

pub mod http;
pub mod jpeg;
pub mod mjpeg;
pub mod onvif;
pub mod rtsp;
pub mod synthetic;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use ss_core::{CameraId, CameraSource, Error, Frame, Result};

use crate::http::SnapshotSource;
use crate::mjpeg::MjpegHttpSource;
use crate::rtsp::RtspSource;
use crate::synthetic::SyntheticSource;

/// A connected stream of frames. `next_frame` returns `Error::Camera` when
/// the stream breaks; the engine then drops the source and reconnects.
#[async_trait]
pub trait FrameSource: Send {
    async fn next_frame(&mut self) -> Result<Frame>;
}

/// Frame rate requested from ffmpeg for RTSP cameras.
const RTSP_FPS: u32 = 10;

/// Build and connect the right source for a camera configuration.
pub async fn connect(camera_id: CameraId, source: &CameraSource) -> Result<Box<dyn FrameSource>> {
    let boxed: Box<dyn FrameSource> = match source {
        CameraSource::Rtsp {
            url,
            username,
            password,
        } => Box::new(
            RtspSource::spawn(
                camera_id,
                url,
                username.as_deref(),
                password.as_deref(),
                RTSP_FPS,
            )
            .await?,
        ),
        CameraSource::Mjpeg {
            url,
            username,
            password,
        } => Box::new(
            MjpegHttpSource::connect(camera_id, url, username.as_deref(), password.as_deref())
                .await?,
        ),
        CameraSource::Snapshot {
            url,
            username,
            password,
            interval_ms,
        } => Box::new(SnapshotSource::new(
            camera_id,
            url,
            username.as_deref(),
            password.as_deref(),
            std::time::Duration::from_millis(*interval_ms),
        )?),
        CameraSource::Synthetic { width, height, fps } => {
            Box::new(SyntheticSource::new(camera_id, *width, *height, *fps))
        }
    };
    Ok(boxed)
}

/// Wrap a JPEG as a frame stamped with the current time. Dimensions come from
/// the JPEG header; they are 0x0 when the header cannot be parsed.
pub(crate) fn make_frame(camera_id: CameraId, jpeg: Bytes) -> Frame {
    let (width, height) = jpeg::dimensions(&jpeg).unwrap_or((0, 0));
    Frame {
        camera_id,
        ts: Utc::now(),
        jpeg,
        width,
        height,
    }
}

/// Shorthand for a camera-side failure.
pub(crate) fn camera_error(msg: impl Into<String>) -> Error {
    Error::Camera(msg.into())
}
