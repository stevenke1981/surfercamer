//! Runtime orchestration: one worker task per enabled camera wiring
//! source → FrameHub → motion → recorder, plus retention and event fan-out.
//! See docs/SPEC.md §9.

mod worker;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use ss_core::*;
use ss_recorder::retention;
use ss_recorder::segment::{SegmentReader, INDEX_RECORD_SIZE};
use tokio::sync::{broadcast, watch, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

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
    /// Running camera workers, by camera id.
    workers: Mutex<HashMap<CameraId, Worker>>,
    /// The retention loop, once started.
    retention: Mutex<Option<Task>>,
    /// Serialises start/stop/reload so two lifecycle operations never interleave.
    lifecycle: AsyncMutex<()>,
}

/// A running camera: its shared state and its tasks.
struct Worker {
    shared: Arc<worker::Shared>,
    task: Task,
}

/// A group of tasks sharing one stop signal.
struct Task {
    stop: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

impl Task {
    /// Signal stop, then wait for every task to finish.
    async fn stop(self) {
        let _ = self.stop.send(true);
        for handle in self.handles {
            if let Err(e) = handle.await {
                error!(error = %e, "engine task ended abnormally");
            }
        }
    }
}

/// Capacity of the event broadcast channel.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Events older than this are removed by the retention sweep.
const EVENT_RETENTION_DAYS: i64 = 90;
/// Lower bound for the retention sweep interval.
const MIN_RETENTION_INTERVAL_SECS: u32 = 10;
/// Page size used when scanning recordings at startup.
const RECORDING_PAGE: u32 = 1000;

/// Lock a std mutex, recovering the data if a holder panicked.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Engine {
    /// Construct without starting anything. Must not panic or touch the network.
    pub fn new(store: Arc<dyn Store>, config: EngineConfig) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                store,
                config,
                frames: FrameHub::new(),
                events,
                workers: Mutex::new(HashMap::new()),
                retention: Mutex::new(None),
                lifecycle: AsyncMutex::new(()),
            }),
        }
    }

    /// Close dangling recordings, start workers for enabled cameras and the
    /// retention loop.
    pub async fn start(&self) -> Result<()> {
        let _guard = self.inner.lifecycle.lock().await;
        self.recover_dangling()?;
        for camera in self.inner.store.list_cameras()? {
            if camera.enabled && !self.has_worker(camera.id) {
                self.launch(camera, false);
            }
        }
        self.start_retention();
        info!("engine started");
        Ok(())
    }

    /// Stop all workers, finishing open segments.
    pub async fn shutdown(&self) {
        let _guard = self.inner.lifecycle.lock().await;
        let retention = lock(&self.inner.retention).take();
        if let Some(task) = retention {
            task.stop().await;
        }
        let workers: Vec<(CameraId, Worker)> = lock(&self.inner.workers).drain().collect();
        for (id, worker) in workers {
            worker.task.stop().await;
            self.inner.frames.clear_latest(id);
        }
        info!("engine stopped");
    }

    /// Stop the camera's worker if one is running, finishing open segments.
    /// Does not consult the store; clears the camera's latest snapshot.
    pub async fn stop_camera(&self, id: CameraId) {
        let _guard = self.inner.lifecycle.lock().await;
        self.stop_worker(id).await;
    }

    /// (Re)load a camera from the store and restart its worker; stops the
    /// worker if the camera is disabled or no longer exists.
    pub async fn reload_camera(&self, id: CameraId) -> Result<()> {
        let _guard = self.inner.lifecycle.lock().await;
        // A manual recording in progress survives the reload.
        let manual = self.worker_manual(id);
        self.stop_worker(id).await;
        match self.inner.store.get_camera(id) {
            Ok(camera) => {
                if camera.enabled {
                    self.launch(camera, manual);
                }
                Ok(())
            }
            Err(Error::NotFound(_)) => {
                self.inner.frames.remove(id);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn set_manual_recording(&self, id: CameraId, on: bool) -> Result<()> {
        let camera = self.inner.store.get_camera(id)?;
        if !camera.enabled {
            return Err(Error::Invalid(format!("camera {id} is disabled")));
        }
        let shared = lock(&self.inner.workers)
            .get(&id)
            .map(|w| w.shared.clone())
            .ok_or_else(|| Error::NotFound(format!("no running worker for camera {id}")))?;
        lock(&shared.policy).set_manual(on);
        shared.wake.notify_one();
        Ok(())
    }

    /// Status of one camera (Disabled if it has no worker).
    pub fn status(&self, id: CameraId) -> CameraStatus {
        lock(&self.inner.workers)
            .get(&id)
            .map(|w| (*lock(&w.shared.status)).clone())
            .unwrap_or_else(|| worker::default_status(id))
    }

    pub fn statuses(&self) -> Vec<CameraStatus> {
        match self.inner.store.list_cameras() {
            Ok(cameras) => cameras.iter().map(|c| self.status(c.id)).collect(),
            Err(e) => {
                warn!(error = %e, "could not list cameras for status");
                lock(&self.inner.workers).values().map(|w| (*lock(&w.shared.status)).clone()).collect()
            }
        }
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
        let store = &self.inner.store;
        let root = self.storage_root().to_path_buf();
        let now = Utc::now();
        let mut deleted = 0u64;

        for camera in store.list_cameras()? {
            let candidates = store.retention_candidates(camera.id)?;
            // The open segment is not a candidate yet, but it occupies space.
            let usage = store.recording_usage(Some(camera.id))? + self.open_bytes(camera.id);
            let ids = retention::plan(&candidates, &camera.recording.retention, usage, now);
            for id in ids {
                let Some(rec) = candidates.iter().find(|r| r.id == id) else {
                    continue;
                };
                // Files first: a row without files is harmless to remove later,
                // but files without a row would be orphaned.
                if let Err(e) = retention::delete_segment_files(&root, &rec.path) {
                    warn!(camera = camera.id, recording = id, error = %e, "retention could not delete files");
                    continue;
                }
                match store.delete_recording(id) {
                    Ok(()) => deleted += 1,
                    Err(Error::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }

        store.delete_events_before(now - chrono::Duration::days(EVENT_RETENTION_DAYS))?;
        if deleted > 0 {
            info!(deleted, "retention sweep removed recordings");
        }
        Ok(deleted)
    }

    // ------------------------------------------------------------ internals

    fn has_worker(&self, id: CameraId) -> bool {
        lock(&self.inner.workers).contains_key(&id)
    }

    /// Manual-recording flag of the running worker for `id`, if any.
    fn worker_manual(&self, id: CameraId) -> bool {
        lock(&self.inner.workers).get(&id).is_some_and(|w| lock(&w.shared.policy).manual())
    }

    /// Bytes in the camera's open segment (0 when not recording).
    fn open_bytes(&self, id: CameraId) -> u64 {
        lock(&self.inner.workers).get(&id).map_or(0, |w| w.shared.open_bytes.load(Ordering::Relaxed))
    }

    /// Spawn a worker for `camera` and register it. Caller holds `lifecycle`.
    fn launch(&self, camera: Camera, manual: bool) {
        let id = camera.id;
        let worker = worker::spawn(self.clone(), camera, manual);
        info!(camera = id, "camera worker started");
        let previous = lock(&self.inner.workers).insert(id, worker);
        if let Some(previous) = previous {
            warn!(camera = id, "replaced a camera worker that was still registered");
            tokio::spawn(previous.task.stop());
        }
    }

    /// Remove and stop the worker for `id`, if any. Caller holds `lifecycle`.
    async fn stop_worker(&self, id: CameraId) {
        let worker = lock(&self.inner.workers).remove(&id);
        if let Some(worker) = worker {
            worker.task.stop().await;
            info!(camera = id, "camera worker stopped");
        }
        // A stopped camera must not keep serving its last snapshot.
        self.inner.frames.clear_latest(id);
    }

    fn start_retention(&self) {
        let mut slot = lock(&self.inner.retention);
        if slot.is_some() {
            return;
        }
        let (stop, rx) = watch::channel(false);
        let handle = tokio::spawn(retention_loop(self.clone(), rx));
        *slot = Some(Task { stop, handles: vec![handle] });
    }

    /// Finish recordings left open by a crash: use the segment's index for the
    /// final numbers, or delete the row when the file is missing or empty.
    fn recover_dangling(&self) -> Result<()> {
        let store = &self.inner.store;
        let root = self.storage_root().to_path_buf();

        let mut dangling = Vec::new();
        let mut offset = 0u32;
        loop {
            let page = store.list_recordings(&RecordingQuery {
                limit: Some(RECORDING_PAGE),
                offset: Some(offset),
                ..Default::default()
            })?;
            let fetched = page.len();
            dangling.extend(page.into_iter().filter(|r| r.end.is_none()));
            if fetched < RECORDING_PAGE as usize {
                break;
            }
            offset = offset.saturating_add(RECORDING_PAGE);
        }

        for rec in dangling {
            let discard = |reason: &str| {
                warn!(recording = rec.id, path = %rec.path, reason, "discarding unfinished recording");
                if let Err(e) = retention::delete_segment_files(&root, &rec.path) {
                    warn!(recording = rec.id, error = %e, "could not delete unfinished segment files");
                }
                if let Err(e) = store.delete_recording(rec.id) {
                    warn!(recording = rec.id, error = %e, "could not delete unfinished recording row");
                }
            };
            match SegmentReader::open(&root, &rec.path) {
                Ok(reader) => {
                    let entries = reader.entries();
                    if entries.is_empty() {
                        discard("segment has no frames");
                        continue;
                    }
                    let frames = entries.len() as u64;
                    let size = entries.iter().map(|e| u64::from(e.len)).sum::<u64>()
                        + frames * INDEX_RECORD_SIZE as u64;
                    let end = rec.start + chrono::Duration::milliseconds(i64::from(reader.duration_ms()));
                    match store.finish_recording(rec.id, end, frames, size) {
                        Ok(_) => info!(recording = rec.id, frames, "recovered unfinished recording"),
                        Err(e) => warn!(recording = rec.id, error = %e, "could not finish recovered recording"),
                    }
                }
                Err(Error::NotFound(_)) => discard("segment file missing"),
                Err(e) => {
                    // Keep the file, but close the row so it does not stay unfinished forever.
                    warn!(recording = rec.id, error = %e, "could not read unfinished segment; closing it empty");
                    if let Err(e) = store.finish_recording(rec.id, rec.start, 0, 0) {
                        warn!(recording = rec.id, error = %e, "could not close unreadable recording row");
                    }
                }
            }
        }
        Ok(())
    }
}

/// Periodic retention sweep; re-reads the interval from settings each round.
async fn retention_loop(engine: Engine, mut stop: watch::Receiver<bool>) {
    loop {
        let interval = engine
            .store()
            .get_settings()
            .map_or(Settings::default().retention_interval_seconds, |s| s.retention_interval_seconds)
            .max(MIN_RETENTION_INTERVAL_SECS);
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(u64::from(interval))) => {}
        }
        if let Err(e) = engine.run_retention().await {
            warn!(error = %e, "retention sweep failed");
        }
    }
}
