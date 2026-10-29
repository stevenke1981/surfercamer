//! Runtime orchestration: one worker task per enabled camera wiring
//! source → FrameHub → motion → recorder, plus retention and event fan-out.
//! See docs/SPEC.md §9.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ss_core::*;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Root directory for segment files.
    pub storage_root: PathBuf,
}

/// Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn Store>,
    config: EngineConfig,
    frames: FrameHub,
    events: broadcast::Sender<Event>,
}

/// Capacity of the event broadcast channel.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

impl Engine {
    /// Construct without starting anything. Must not panic or touch the network.
    pub fn new(store: Arc<dyn Store>, config: EngineConfig) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { inner: Arc::new(Inner { store, config, frames: FrameHub::new(), events }) }
    }

    /// Close dangling recordings, start workers for enabled cameras and the
    /// retention loop.
    pub async fn start(&self) -> Result<()> {
        todo!()
    }

    /// Stop all workers, finishing open segments.
    pub async fn shutdown(&self) {
        todo!()
    }

    /// (Re)load a camera from the store and restart its worker; stops the
    /// worker if the camera is disabled or no longer exists.
    pub async fn reload_camera(&self, id: CameraId) -> Result<()> {
        let _ = id;
        todo!()
    }

    pub async fn set_manual_recording(&self, id: CameraId, on: bool) -> Result<()> {
        let _ = (id, on);
        todo!()
    }

    /// Status of one camera (Disabled if it has no worker).
    pub fn status(&self, id: CameraId) -> CameraStatus {
        let _ = id;
        todo!()
    }

    pub fn statuses(&self) -> Vec<CameraStatus> {
        todo!()
    }

    pub fn frames(&self) -> &FrameHub {
        &self.inner.frames
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.inner.store
    }

    pub fn storage_root(&self) -> &Path {
        &self.inner.config.storage_root
    }

    /// Persist an event and broadcast it to subscribers.
    pub fn emit_event(&self, ev: NewEvent) -> Result<Event> {
        let event = self.inner.store.create_event(&ev)?;
        let _ = self.inner.events.send(event.clone());
        Ok(event)
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Run one retention sweep across all cameras; returns recordings deleted.
    pub async fn run_retention(&self) -> Result<u64> {
        todo!()
    }
}
