//! Per-camera tasks: the supervisor (connect, frame loop, reconnect), the
//! motion analyser and the recorder. All three share one [`Shared`] block that
//! the registry in `lib.rs` also reads for `status()` and manual recording.
//! See docs/SPEC.md §9.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use ss_camera::FrameSource;
use ss_core::{
    Camera, CameraId, CameraStatus, ConnectionState, Error, EventKind, Frame, NewEvent, NewRecording,
    RecordingId, RecordingTrigger, Result, Schedule, Settings, Store,
};
use ss_motion::MotionDetector;
use ss_recorder::disk::disk_space;
use ss_recorder::prebuffer::PreBuffer;
use ss_recorder::retention::delete_segment_files;
use ss_recorder::segment::{segment_rel_path, SegmentWriter};
use ss_recorder::RecordPolicy;
use tokio::sync::{broadcast, watch};
use tracing::{debug, error, info, warn};

use crate::{lock, Engine, Task, Worker};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Motion analysis runs at most this often.
const ANALYSIS_INTERVAL: Duration = Duration::from_millis(200);
/// Re-evaluate the record decision this often even without frames.
const EVALUATE_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum gap between two StorageFull events.
const STORAGE_FULL_REPEAT: chrono::Duration = chrono::Duration::minutes(10);
/// Attempts at finding an unused segment file name.
const MAX_SEGMENT_ATTEMPTS: u32 = 100;

/// State shared between a camera's tasks and the engine registry.
pub(crate) struct Shared {
    pub(crate) status: Mutex<CameraStatus>,
    pub(crate) policy: Mutex<RecordPolicy>,
    pub(crate) open_recording: Mutex<Option<RecordingId>>,
    /// Wakes the recorder for an immediate re-evaluation (manual record toggle).
    pub(crate) wake: tokio::sync::Notify,
}

impl Shared {
    pub(crate) fn new(camera_id: CameraId) -> Self {
        Self {
            status: Mutex::new(default_status(camera_id)),
            policy: Mutex::new(RecordPolicy::new()),
            open_recording: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
        }
    }
}

/// Status reported for a camera without a running worker.
pub(crate) fn default_status(camera_id: CameraId) -> CameraStatus {
    CameraStatus {
        camera_id,
        state: ConnectionState::Disabled,
        recording: false,
        recording_trigger: None,
        motion_active: false,
        fps: 0.0,
        last_frame_at: None,
        last_error: None,
    }
}

/// Spawn the tasks for one camera. Must be called inside a Tokio runtime.
pub(crate) fn spawn(engine: Engine, camera: Camera) -> Worker {
    let shared = Arc::new(Shared::new(camera.id));
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut handles = Vec::with_capacity(3);

    // Subscribe before any task starts so no early frame is missed.
    if camera.motion.enabled {
        let frames = engine.frames().subscribe(camera.id);
        handles.push(tokio::spawn(run_motion(
            engine.clone(),
            camera.clone(),
            shared.clone(),
            frames,
            stop_rx.clone(),
        )));
    }
    let frames = engine.frames().subscribe(camera.id);
    let root = engine.storage_root().to_path_buf();
    handles.push(tokio::spawn(run_recorder(
        engine.clone(),
        camera.clone(),
        shared.clone(),
        root,
        frames,
        stop_rx.clone(),
    )));
    handles.push(tokio::spawn(supervise(engine, camera, shared.clone(), stop_rx)));

    Worker { shared, task: Task { stop: stop_tx, handles } }
}

/// Publish an event through the engine, logging (not propagating) failures.
pub(crate) fn emit(
    engine: &Engine,
    camera_id: CameraId,
    kind: EventKind,
    message: String,
    score: f32,
    recording_id: Option<RecordingId>,
) {
    let ev = NewEvent { camera_id: Some(camera_id), kind, at: Utc::now(), score, message, recording_id };
    if let Err(e) = engine.emit_event(ev) {
        warn!(camera = camera_id, error = %e, "failed to emit event");
    }
}

// ------------------------------------------------------------ supervisor

enum ReadOutcome {
    Stopped,
    Failed(String),
}

/// Connect, read frames, publish them, and reconnect with backoff until stopped.
async fn supervise(engine: Engine, camera: Camera, shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let id = camera.id;
    let mut backoff = MIN_BACKOFF;
    // True between a connection loss being announced and the next first frame.
    let mut lost = false;
    loop {
        set_state(&shared, ConnectionState::Connecting);
        let connected = tokio::select! {
            _ = stop.changed() => break,
            r = ss_camera::connect(id, &camera.source) => r,
        };
        let failure = match connected {
            Ok(mut source) => {
                match read_frames(&engine, id, &shared, source.as_mut(), &mut stop, &mut lost, &mut backoff).await {
                    ReadOutcome::Stopped => break,
                    ReadOutcome::Failed(msg) => msg,
                }
            }
            Err(e) => e.to_string(),
        };
        announce_failure(&engine, id, &shared, &mut lost, &failure);
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    set_state(&shared, ConnectionState::Disabled);
    debug!(camera = id, "supervisor stopped");
}

async fn read_frames(
    engine: &Engine,
    id: CameraId,
    shared: &Shared,
    source: &mut dyn FrameSource,
    stop: &mut watch::Receiver<bool>,
    lost: &mut bool,
    backoff: &mut Duration,
) -> ReadOutcome {
    let mut online = false;
    let mut fps = FpsMeter::default();
    loop {
        let next = tokio::select! {
            _ = stop.changed() => return ReadOutcome::Stopped,
            r = source.next_frame() => r,
        };
        match next {
            Ok(frame) => {
                if !online {
                    online = true;
                    *backoff = MIN_BACKOFF;
                    {
                        let mut st = lock(&shared.status);
                        st.state = ConnectionState::Online;
                        st.last_error = None;
                    }
                    if std::mem::replace(lost, false) {
                        emit(engine, id, EventKind::ConnectionRestored, "Connection restored".into(), 0.0, None);
                        info!(camera = id, "camera back online");
                    }
                }
                let fps_now = fps.tick();
                {
                    let mut st = lock(&shared.status);
                    st.fps = fps_now;
                    st.last_frame_at = Some(frame.ts);
                }
                engine.frames().publish(frame);
            }
            Err(e) => return ReadOutcome::Failed(e.to_string()),
        }
    }
}

fn announce_failure(engine: &Engine, id: CameraId, shared: &Shared, lost: &mut bool, msg: &str) {
    {
        let mut st = lock(&shared.status);
        st.state = ConnectionState::Offline;
        st.fps = 0.0;
        st.last_error = Some(msg.to_string());
    }
    if !*lost {
        *lost = true;
        emit(engine, id, EventKind::ConnectionLost, format!("Connection lost: {msg}"), 0.0, None);
    }
    warn!(camera = id, error = msg, "camera unavailable, will retry");
}

fn set_state(shared: &Shared, state: ConnectionState) {
    lock(&shared.status).state = state;
}

#[derive(Default)]
struct FpsMeter {
    last: Option<Instant>,
    fps: f32,
}

impl FpsMeter {
    /// Record one frame and return the smoothed frame rate.
    fn tick(&mut self) -> f32 {
        let now = Instant::now();
        if let Some(last) = self.last {
            let dt = now.duration_since(last).as_secs_f32();
            if dt > 0.0 {
                let inst = 1.0 / dt;
                self.fps = if self.fps > 0.0 { self.fps * 0.8 + inst * 0.2 } else { inst };
            }
        }
        self.last = Some(now);
        self.fps
    }
}

// ------------------------------------------------------------ motion

async fn run_motion(
    engine: Engine,
    camera: Camera,
    shared: Arc<Shared>,
    mut frames: broadcast::Receiver<Arc<Frame>>,
    mut stop: watch::Receiver<bool>,
) {
    let id = camera.id;
    let cooldown = chrono::Duration::seconds(i64::from(camera.motion.cooldown_seconds));
    let mut detector = MotionDetector::new(camera.motion.clone());
    let mut last_analysed: Option<Instant> = None;
    let mut last_event: Option<DateTime<Utc>> = None;

    loop {
        tokio::select! {
            _ = stop.changed() => break,
            received = frames.recv() => match received {
                Ok(frame) => {
                    let tick = Instant::now();
                    if last_analysed.is_some_and(|prev| tick.duration_since(prev) < ANALYSIS_INTERVAL) {
                        continue;
                    }
                    last_analysed = Some(tick);
                    let result = match detector.process_jpeg(&frame.jpeg) {
                        Ok(r) => r,
                        Err(e) => {
                            debug!(camera = id, error = %e, "motion analysis skipped frame");
                            continue;
                        }
                    };
                    if !result.triggered {
                        continue;
                    }
                    let at = frame.ts;
                    lock(&shared.policy).on_motion(at);
                    lock(&shared.status).motion_active = true;
                    if last_event.is_some_and(|prev| at - prev < cooldown) {
                        continue;
                    }
                    last_event = Some(at);
                    let recording_id = *lock(&shared.open_recording);
                    emit(&engine, id, EventKind::Motion, "Motion detected".into(), result.score, recording_id);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!(camera = id, skipped, "motion task lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    debug!(camera = id, "motion task stopped");
}

// ------------------------------------------------------------ recorder

async fn run_recorder(
    engine: Engine,
    camera: Camera,
    shared: Arc<Shared>,
    root: PathBuf,
    mut frames: broadcast::Receiver<Arc<Frame>>,
    mut stop: watch::Receiver<bool>,
) {
    let shared_for_wake = shared.clone();
    let mut recorder = Recorder::new(engine, &camera, shared, root);
    let mut tick = tokio::time::interval(EVALUATE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            received = frames.recv() => match received {
                Ok(frame) => recorder.on_frame(&frame),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!(camera = camera.id, skipped, "recorder lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => recorder.evaluate(Utc::now(), None),
            _ = shared_for_wake.wake.notified() => recorder.evaluate(Utc::now(), None),
        }
    }
    // Finish the open segment (if any) so its row and index are consistent.
    recorder.close(false);
    debug!(camera = camera.id, "recorder stopped");
}

struct OpenSegment {
    id: RecordingId,
    writer: SegmentWriter,
    trigger: RecordingTrigger,
}

struct Recorder {
    engine: Engine,
    shared: Arc<Shared>,
    camera_id: CameraId,
    root: PathBuf,
    schedule: Schedule,
    post_secs: u32,
    segment_len: chrono::Duration,
    /// Minimum time between accepted frames (max_fps throttle, 10 % tolerance).
    min_gap: chrono::Duration,
    pre: PreBuffer,
    last_accepted: Option<DateTime<Utc>>,
    /// Timestamp of the newest frame written to any segment.
    last_written: Option<DateTime<Utc>>,
    open: Option<OpenSegment>,
    last_storage_full: Option<DateTime<Utc>>,
}

impl Recorder {
    fn new(engine: Engine, camera: &Camera, shared: Arc<Shared>, root: PathBuf) -> Self {
        let rc = &camera.recording;
        let max_fps = i64::from(rc.max_fps.clamp(1, 60));
        Self {
            engine,
            shared,
            camera_id: camera.id,
            root,
            schedule: rc.schedule.clone(),
            post_secs: rc.post_record_seconds,
            segment_len: chrono::Duration::seconds(i64::from(rc.segment_seconds)),
            min_gap: chrono::Duration::milliseconds(1000 / max_fps * 9 / 10),
            pre: PreBuffer::new(rc.pre_record_seconds),
            last_accepted: None,
            last_written: None,
            open: None,
            last_storage_full: None,
        }
    }

    fn store(&self) -> &Arc<dyn Store> {
        self.engine.store()
    }

    fn on_frame(&mut self, frame: &Frame) {
        let ts = frame.ts;
        if self.last_accepted.is_some_and(|prev| ts - prev < self.min_gap) {
            return;
        }
        self.last_accepted = Some(ts);
        self.pre.push(ts, frame.jpeg.clone());
        self.evaluate(Utc::now(), Some((ts, frame.jpeg.clone())));
    }

    /// Apply the record policy at `now`, opening, rotating or closing segments.
    /// `frame` is the newly received frame to write, if any.
    fn evaluate(&mut self, now: DateTime<Utc>, frame: Option<(DateTime<Utc>, Bytes)>) {
        let mode = self.schedule.mode_at(&now);
        let (trigger, motion_active) = {
            let policy = lock(&self.shared.policy);
            (policy.decide(now, mode, self.post_secs), policy.motion_active(now, self.post_secs))
        };
        lock(&self.shared.status).motion_active = motion_active;

        let Some(trigger) = trigger else {
            if self.open.is_some() {
                self.close(true);
            }
            return;
        };

        let frame_ts = frame.as_ref().map_or(now, |(ts, _)| *ts);
        let rotate = match &self.open {
            None => true,
            Some(open) => open.trigger != trigger || frame_ts - open.writer.start() >= self.segment_len,
        };
        if rotate {
            let from_idle = self.open.is_none();
            if self.open.is_some() {
                self.close(false);
            }
            // Segments are opened lazily, when a frame is available to write.
            let Some((ts, _)) = frame.as_ref() else {
                return;
            };
            let ts = *ts;
            if !self.storage_ok(now) {
                return;
            }
            if !self.open_segment(trigger, ts, from_idle) {
                return;
            }
        }
        if let Some((ts, jpeg)) = frame {
            self.write(ts, &jpeg);
        }
    }

    /// Open a segment; drains the pre-buffer so buffered frames are not lost.
    fn open_segment(&mut self, trigger: RecordingTrigger, frame_ts: DateTime<Utc>, announce: bool) -> bool {
        let newest_written = self.last_written;
        let pending: Vec<(DateTime<Utc>, Bytes)> = self
            .pre
            .drain()
            .into_iter()
            .filter(|(ts, _)| newest_written.is_none_or(|lw| *ts > lw))
            .collect();
        let start = pending.first().map_or(frame_ts, |(ts, _)| *ts);

        let (writer, id) = match self.create_segment(start, trigger) {
            Ok(v) => v,
            Err(e) => {
                error!(camera = self.camera_id, error = %e, "failed to open recording");
                lock(&self.shared.status).last_error = Some(format!("recording failed: {e}"));
                return false;
            }
        };
        self.open = Some(OpenSegment { id, writer, trigger });
        *lock(&self.shared.open_recording) = Some(id);
        {
            let mut st = lock(&self.shared.status);
            st.recording = true;
            st.recording_trigger = Some(trigger);
        }
        if announce {
            emit(
                &self.engine,
                self.camera_id,
                EventKind::RecordingStarted,
                format!("Recording started ({})", trigger.as_str()),
                0.0,
                Some(id),
            );
        }
        info!(camera = self.camera_id, recording = id, trigger = trigger.as_str(), "segment opened");
        for (ts, jpeg) in pending {
            self.write(ts, &jpeg);
        }
        true
    }

    /// Create the segment file and its row. A file name clash (two segments
    /// starting in the same millisecond) is resolved by moving the start forward.
    fn create_segment(&self, start: DateTime<Utc>, trigger: RecordingTrigger) -> Result<(SegmentWriter, RecordingId)> {
        let mut t = start;
        for _ in 0..MAX_SEGMENT_ATTEMPTS {
            let rel = segment_rel_path(self.camera_id, t);
            let writer = match SegmentWriter::create(&self.root, &rel, t) {
                Ok(w) => w,
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    t += chrono::Duration::milliseconds(1);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let new = NewRecording { camera_id: self.camera_id, path: rel.clone(), start: t, trigger };
            return match self.store().create_recording(&new) {
                Ok(rec) => Ok((writer, rec.id)),
                Err(e) => {
                    drop(writer);
                    if let Err(clean) = delete_segment_files(&self.root, &rel) {
                        warn!(error = %clean, "could not remove orphan segment file");
                    }
                    Err(e)
                }
            };
        }
        Err(Error::Storage("could not allocate a unique segment file name".into()))
    }

    /// Append a frame to the open segment, skipping frames already written.
    fn write(&mut self, ts: DateTime<Utc>, jpeg: &[u8]) {
        if self.last_written.is_some_and(|lw| ts <= lw) {
            return;
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        match open.writer.write_frame(ts, jpeg) {
            Ok(()) => self.last_written = Some(ts),
            Err(e) => {
                error!(camera = self.camera_id, error = %e, "segment write failed, closing segment");
                lock(&self.shared.status).last_error = Some(format!("recording write failed: {e}"));
                self.close(true);
            }
        }
    }

    /// Finish the open segment and record its final size in the store.
    fn close(&mut self, announce: bool) {
        let Some(open) = self.open.take() else {
            return;
        };
        let frames = open.writer.frames();
        let size = open.writer.size_bytes();
        let end = open.writer.last_ts();
        if let Err(e) = open.writer.finish() {
            warn!(camera = self.camera_id, recording = open.id, error = %e, "segment close reported an error");
        }
        if let Err(e) = self.store().finish_recording(open.id, end, frames, size) {
            error!(camera = self.camera_id, recording = open.id, error = %e, "failed to finish recording row");
        }
        *lock(&self.shared.open_recording) = None;
        {
            let mut st = lock(&self.shared.status);
            st.recording = false;
            st.recording_trigger = None;
        }
        info!(camera = self.camera_id, recording = open.id, frames, "segment closed");
        if announce {
            emit(
                &self.engine,
                self.camera_id,
                EventKind::RecordingStopped,
                format!("Recording stopped ({})", open.trigger.as_str()),
                0.0,
                Some(open.id),
            );
        }
    }

    /// False (and a rate-limited StorageFull event) when free space is below the limit.
    fn storage_ok(&mut self, now: DateTime<Utc>) -> bool {
        let settings = match self.store().get_settings() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "could not read settings; assuming free space");
                Settings::default()
            }
        };
        let free = match disk_space(&self.root) {
            Ok(space) => space.free_bytes,
            Err(e) => {
                warn!(error = %e, "could not query disk space; assuming free");
                return true;
            }
        };
        if free >= settings.min_free_bytes {
            return true;
        }
        if self.last_storage_full.is_none_or(|prev| now - prev >= STORAGE_FULL_REPEAT) {
            self.last_storage_full = Some(now);
            emit(
                &self.engine,
                self.camera_id,
                EventKind::StorageFull,
                format!("Storage full: {free} bytes free, {} required", settings.min_free_bytes),
                0.0,
                None,
            );
        }
        false
    }
}
