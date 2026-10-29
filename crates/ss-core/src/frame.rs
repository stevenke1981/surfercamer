use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::model::CameraId;

/// One JPEG-encoded video frame. All sources normalise to JPEG so that
/// motion detection, live view (MJPEG) and recording share one format.
#[derive(Debug, Clone)]
pub struct Frame {
    pub camera_id: CameraId,
    pub ts: DateTime<Utc>,
    pub jpeg: Bytes,
    pub width: u32,
    pub height: u32,
}

/// Capacity of each per-camera broadcast channel. Slow subscribers lag and
/// skip frames rather than block the source.
pub const FRAME_CHANNEL_CAPACITY: usize = 32;

/// Fan-out of frames per camera. Sources publish; live view, motion detection
/// and the recorder subscribe. Also keeps the latest frame for snapshots.
#[derive(Default, Clone)]
pub struct FrameHub {
    inner: Arc<Mutex<HashMap<CameraId, Channel>>>,
}

struct Channel {
    tx: broadcast::Sender<Arc<Frame>>,
    latest: Option<Arc<Frame>>,
}

impl FrameHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_channel<R>(&self, id: CameraId, f: impl FnOnce(&mut Channel) -> R) -> R {
        let mut map = self.inner.lock().expect("frame hub poisoned");
        let ch = map.entry(id).or_insert_with(|| Channel {
            tx: broadcast::channel(FRAME_CHANNEL_CAPACITY).0,
            latest: None,
        });
        f(ch)
    }

    /// Publish a frame; returns the number of active subscribers.
    pub fn publish(&self, frame: Frame) -> usize {
        let frame = Arc::new(frame);
        self.with_channel(frame.camera_id, |ch| {
            ch.latest = Some(frame.clone());
            ch.tx.send(frame).unwrap_or(0)
        })
    }

    pub fn subscribe(&self, id: CameraId) -> broadcast::Receiver<Arc<Frame>> {
        self.with_channel(id, |ch| ch.tx.subscribe())
    }

    pub fn latest(&self, id: CameraId) -> Option<Arc<Frame>> {
        self.with_channel(id, |ch| ch.latest.clone())
    }

    /// Forget the latest frame (camera stopped or offline) so snapshots
    /// don't serve stale images. Subscribers stay connected.
    pub fn clear_latest(&self, id: CameraId) {
        if let Some(ch) = self.inner.lock().expect("frame hub poisoned").get_mut(&id) {
            ch.latest = None;
        }
    }

    /// Drop a camera's channel (on camera delete). Subscribers see `Closed`.
    pub fn remove(&self, id: CameraId) {
        self.inner.lock().expect("frame hub poisoned").remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: CameraId) -> Frame {
        Frame {
            camera_id: id,
            ts: Utc::now(),
            jpeg: Bytes::from_static(b"x"),
            width: 1,
            height: 1,
        }
    }

    #[tokio::test]
    async fn publish_reaches_subscriber_and_latest() {
        let hub = FrameHub::new();
        let mut rx = hub.subscribe(1);
        assert_eq!(hub.publish(frame(1)), 1);
        assert_eq!(rx.recv().await.unwrap().camera_id, 1);
        assert!(hub.latest(1).is_some());
        assert!(hub.latest(2).is_none());
    }
}
