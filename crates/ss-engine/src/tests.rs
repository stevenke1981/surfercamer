//! Engine tests: synthetic cameras, a real SQLite store and temp segment dirs.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use ss_core::*;
use ss_db::SqliteStore;
use ss_recorder::segment::{segment_rel_path, SegmentReader, SegmentWriter};
use tokio::sync::broadcast::error::TryRecvError;

use crate::{Engine, EngineConfig};

fn new_store() -> Arc<dyn Store> {
    Arc::new(SqliteStore::open_in_memory().expect("in-memory store"))
}

fn synthetic(name: &str, schedule: Schedule, enabled: bool, motion: MotionConfig) -> CameraInput {
    CameraInput {
        name: name.into(),
        enabled,
        source: CameraSource::Synthetic { width: 160, height: 120, fps: 10 },
        recording: RecordingConfig {
            schedule,
            segment_seconds: 10,
            pre_record_seconds: 2,
            post_record_seconds: 2,
            max_fps: 10,
            retention: RetentionPolicy::default(),
        },
        motion,
    }
}

fn engine_in(store: &Arc<dyn Store>, dir: &Path) -> Engine {
    Engine::new(store.clone(), EngineConfig { storage_root: dir.to_path_buf() })
}

/// Poll `cond` every 50 ms for up to `secs` seconds.
async fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

fn recordings_of(store: &Arc<dyn Store>, camera_id: CameraId) -> Vec<Recording> {
    store
        .list_recordings(&RecordingQuery { camera_id: Some(camera_id), ..Default::default() })
        .expect("list recordings")
}

fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<Event> {
    let mut out = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(ev) => out.push(ev),
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    out
}

/// Write a finished segment with three frames at `start` and return its path and summary.
fn write_segment(root: &Path, camera_id: CameraId, start: DateTime<Utc>) -> (String, ss_recorder::segment::SegmentSummary) {
    let rel = segment_rel_path(camera_id, start);
    let mut w = SegmentWriter::create(root, &rel, start).expect("create segment");
    for i in 0..3 {
        w.write_frame(start + ChronoDuration::milliseconds(i * 100), b"\xFF\xD8test\xFF\xD9")
            .expect("write frame");
    }
    (rel, w.finish().expect("finish segment"))
}

/// Finished recording row backed by a real segment file.
fn finished_recording(store: &Arc<dyn Store>, root: &Path, camera_id: CameraId, start: DateTime<Utc>) -> Recording {
    let (rel, summary) = write_segment(root, camera_id, start);
    let rec = store
        .create_recording(&NewRecording { camera_id, path: rel, start, trigger: RecordingTrigger::Continuous })
        .expect("create recording");
    store
        .finish_recording(rec.id, summary.end, summary.frames, summary.size_bytes)
        .expect("finish recording")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_camera_is_disabled_and_statuses_empty() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_in(&new_store(), dir.path());
    let st = engine.status(42);
    assert_eq!(st.camera_id, 42);
    assert_eq!(st.state, ConnectionState::Disabled);
    assert!(!st.recording);
    assert!(st.recording_trigger.is_none());
    assert!(!st.motion_active);
    assert!(engine.statuses().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_camera_appears_in_statuses_as_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let cam = store
        .create_camera(&synthetic("off", Schedule::default(), false, MotionConfig::default()))
        .unwrap();
    let engine = engine_in(&store, dir.path());
    engine.start().await.unwrap();
    let all = engine.statuses();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].camera_id, cam.id);
    assert_eq!(all[0].state, ConnectionState::Disabled);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_camera_goes_online_records_and_shutdown_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let motion = MotionConfig { enabled: false, ..MotionConfig::default() };
    let cam = store
        .create_camera(&synthetic("front", Schedule::uniform(ScheduleMode::Continuous), true, motion))
        .unwrap();
    let engine = engine_in(&store, dir.path());
    let mut events = engine.subscribe_events();
    engine.start().await.unwrap();

    assert!(wait_for(10, || engine.status(cam.id).state == ConnectionState::Online).await, "camera never went online");
    let mut frames = engine.frames().subscribe(cam.id);
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..5 {
            frames.recv().await.expect("frame");
        }
    })
    .await;
    assert!(got.is_ok(), "fewer than 5 frames published");
    assert!(engine.frames().latest(cam.id).is_some());

    assert!(wait_for(5, || !recordings_of(&store, cam.id).is_empty()).await, "no recording row");
    let st = engine.status(cam.id);
    assert!(st.recording);
    assert_eq!(st.recording_trigger, Some(RecordingTrigger::Continuous));
    assert!(st.fps > 0.0);
    assert!(st.last_frame_at.is_some());

    tokio::time::sleep(Duration::from_millis(1500)).await;
    engine.shutdown().await;

    let recs = recordings_of(&store, cam.id);
    assert_eq!(recs.len(), 1, "one segment expected: {recs:?}");
    let rec = &recs[0];
    assert!(rec.end.is_some(), "segment left open after shutdown");
    assert!(rec.frames > 0);
    assert_eq!(rec.trigger, RecordingTrigger::Continuous);
    assert!(dir.path().join(&rec.path).exists());

    let mut reader = SegmentReader::open(dir.path(), &rec.path).expect("open segment");
    assert_eq!(reader.entries().len() as u64, rec.frames);
    let first = reader.read_frame(0).expect("read frame");
    assert_eq!(&first[..2], &[0xFF, 0xD8], "frame is not a JPEG");

    let evs = drain_events(&mut events);
    assert!(evs.iter().any(|e| e.kind == EventKind::RecordingStarted && e.camera_id == Some(cam.id)));
    assert_eq!(engine.status(cam.id).state, ConnectionState::Disabled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_schedule_produces_motion_events_and_motion_recording() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let motion = MotionConfig { cooldown_seconds: 1, ..MotionConfig::default() };
    let cam = store
        .create_camera(&synthetic("moving", Schedule::uniform(ScheduleMode::Motion), true, motion))
        .unwrap();
    let engine = engine_in(&store, dir.path());
    let mut events = engine.subscribe_events();
    engine.start().await.unwrap();

    let mut seen_motion = Vec::new();
    let found = wait_for(15, || {
        seen_motion.extend(drain_events(&mut events).into_iter().filter(|e| e.kind == EventKind::Motion));
        recordings_of(&store, cam.id).iter().any(|r| r.trigger == RecordingTrigger::Motion)
    })
    .await;
    assert!(found, "no motion recording within 15 s");
    seen_motion.extend(drain_events(&mut events).into_iter().filter(|e| e.kind == EventKind::Motion));
    assert!(!seen_motion.is_empty(), "no motion events emitted");
    assert!(seen_motion.iter().all(|e| e.camera_id == Some(cam.id) && e.score > 0.0));

    engine.shutdown().await;
    let recs = recordings_of(&store, cam.id);
    assert!(recs.iter().filter(|r| r.trigger == RecordingTrigger::Motion).all(|r| r.end.is_some()));
    assert!(recs.iter().all(|r| r.trigger == RecordingTrigger::Motion), "unexpected trigger in {recs:?}");

    // The first motion event starts the motion recording; it must be linked to it.
    let stored = store
        .list_events(&EventQuery { camera_id: Some(cam.id), kind: Some(EventKind::Motion), ..Default::default() })
        .expect("list events");
    let first = stored.iter().min_by_key(|e| e.at).expect("stored motion event");
    let linked = first.recording_id.expect("first motion event must reference its recording");
    let rec = store.get_recording(linked).expect("linked recording exists");
    assert_eq!(rec.trigger, RecordingTrigger::Motion);
    assert_eq!(rec.camera_id, cam.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_recording_on_schedule_none_camera() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let motion = MotionConfig { enabled: false, ..MotionConfig::default() };
    let cam = store
        .create_camera(&synthetic("manual", Schedule::uniform(ScheduleMode::None), true, motion))
        .unwrap();
    let off = store
        .create_camera(&synthetic("off", Schedule::default(), false, MotionConfig::default()))
        .unwrap();
    let engine = engine_in(&store, dir.path());
    engine.start().await.unwrap();
    assert!(wait_for(10, || engine.status(cam.id).state == ConnectionState::Online).await);

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(recordings_of(&store, cam.id).is_empty(), "schedule None must not record");
    assert!(!engine.status(cam.id).recording);

    engine.set_manual_recording(cam.id, true).await.unwrap();
    assert!(wait_for(5, || engine.status(cam.id).recording).await, "manual recording did not start");
    assert_eq!(engine.status(cam.id).recording_trigger, Some(RecordingTrigger::Manual));
    tokio::time::sleep(Duration::from_millis(800)).await;

    engine.set_manual_recording(cam.id, false).await.unwrap();
    assert!(wait_for(5, || !engine.status(cam.id).recording).await, "manual recording did not stop");
    let recs = recordings_of(&store, cam.id);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].trigger, RecordingTrigger::Manual);
    assert!(recs[0].end.is_some() && recs[0].frames > 0);

    assert!(matches!(engine.set_manual_recording(off.id, true).await, Err(Error::Invalid(_))));
    assert!(matches!(engine.set_manual_recording(999_999, true).await, Err(Error::NotFound(_))));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_with_camera_disabled_stops_worker() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let motion = MotionConfig { enabled: false, ..MotionConfig::default() };
    let input = synthetic("toggle", Schedule::uniform(ScheduleMode::Continuous), true, motion.clone());
    let cam = store.create_camera(&input).unwrap();
    let engine = engine_in(&store, dir.path());
    engine.start().await.unwrap();
    assert!(wait_for(10, || engine.status(cam.id).state == ConnectionState::Online).await);

    let mut disabled = input.clone();
    disabled.enabled = false;
    store.update_camera(cam.id, &disabled).unwrap();
    engine.reload_camera(cam.id).await.unwrap();

    assert_eq!(engine.status(cam.id).state, ConnectionState::Disabled);
    assert!(!engine.has_worker(cam.id), "worker still registered after disable");
    assert!(!engine.status(cam.id).recording);
    // No more frames once stopped.
    let before = engine.frames().latest(cam.id).map(|f| f.ts);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = engine.frames().latest(cam.id).map(|f| f.ts);
    assert_eq!(before, after, "frames still arriving after the worker stopped");

    // Re-enabling starts a worker again.
    store.update_camera(cam.id, &input).unwrap();
    engine.reload_camera(cam.id).await.unwrap();
    assert!(wait_for(10, || engine.status(cam.id).state == ConnectionState::Online).await);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_recovers_dangling_recordings() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let cam = store
        .create_camera(&synthetic("crashed", Schedule::default(), false, MotionConfig::default()))
        .unwrap();
    let start = Utc.with_ymd_and_hms(2026, 5, 1, 10, 0, 0).unwrap();

    // Crash survivor: file exists, row never finished.
    let (rel, summary) = write_segment(dir.path(), cam.id, start);
    let survivor = store
        .create_recording(&NewRecording { camera_id: cam.id, path: rel.clone(), start, trigger: RecordingTrigger::Motion })
        .unwrap();
    // Row whose file never made it to disk.
    let ghost = store
        .create_recording(&NewRecording {
            camera_id: cam.id,
            path: "cam1/2026-05-01/000000-000.mjpg".into(),
            start,
            trigger: RecordingTrigger::Continuous,
        })
        .unwrap();

    let engine = engine_in(&store, dir.path());
    engine.start().await.unwrap();
    engine.shutdown().await;

    let recs = recordings_of(&store, cam.id);
    assert_eq!(recs.len(), 1, "ghost row should be removed: {recs:?}");
    let rec = &recs[0];
    assert_eq!(rec.id, survivor.id);
    assert_ne!(rec.id, ghost.id);
    assert_eq!(rec.frames, summary.frames);
    assert_eq!(rec.size_bytes, summary.size_bytes);
    assert_eq!(rec.end, Some(start + ChronoDuration::milliseconds(200)));
    assert!(dir.path().join(&rel).exists(), "survivor file must be kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_deletes_old_unlocked_and_keeps_locked() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let cam = store
        .create_camera(&synthetic("keep", Schedule::default(), false, MotionConfig::default()))
        .unwrap();
    let now = Utc::now();
    let old = finished_recording(&store, dir.path(), cam.id, now - ChronoDuration::days(60));
    let old_locked = finished_recording(&store, dir.path(), cam.id, now - ChronoDuration::days(61));
    store.set_recording_locked(old_locked.id, true).unwrap();
    let recent = finished_recording(&store, dir.path(), cam.id, now - ChronoDuration::days(1));

    let engine = engine_in(&store, dir.path());
    let deleted = engine.run_retention().await.unwrap();
    assert_eq!(deleted, 1);

    assert!(store.get_recording(old.id).is_err(), "old unlocked row should be gone");
    assert!(!dir.path().join(&old.path).exists(), "old unlocked file should be gone");
    assert!(store.get_recording(old_locked.id).is_ok(), "locked row must stay");
    assert!(dir.path().join(&old_locked.path).exists(), "locked file must stay");
    assert!(store.get_recording(recent.id).is_ok(), "recent row must stay");
    assert!(dir.path().join(&recent.path).exists());

    // Second sweep finds nothing to do.
    assert_eq!(engine.run_retention().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_recording_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let motion = MotionConfig { enabled: false, ..MotionConfig::default() };
    let cam = store
        .create_camera(&synthetic("manual-reload", Schedule::uniform(ScheduleMode::None), true, motion))
        .unwrap();
    let engine = engine_in(&store, dir.path());
    engine.start().await.unwrap();
    assert!(wait_for(10, || engine.status(cam.id).state == ConnectionState::Online).await);

    engine.set_manual_recording(cam.id, true).await.unwrap();
    assert!(wait_for(5, || engine.status(cam.id).recording).await, "manual recording did not start");

    // Settings change while recording: the manual request must still be honoured.
    engine.reload_camera(cam.id).await.unwrap();
    assert!(
        wait_for(10, || {
            let st = engine.status(cam.id);
            st.recording && st.recording_trigger == Some(RecordingTrigger::Manual)
        })
        .await,
        "manual recording lost on reload"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_camera_stops_worker_and_clears_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let motion = MotionConfig { enabled: false, ..MotionConfig::default() };
    let cam = store
        .create_camera(&synthetic("stopping", Schedule::uniform(ScheduleMode::Continuous), true, motion))
        .unwrap();
    let engine = engine_in(&store, dir.path());
    engine.start().await.unwrap();
    assert!(wait_for(10, || engine.frames().latest(cam.id).is_some()).await, "no frame published");
    assert!(wait_for(5, || !recordings_of(&store, cam.id).is_empty()).await, "no recording row");

    engine.stop_camera(cam.id).await;

    assert!(!engine.has_worker(cam.id), "worker still registered after stop_camera");
    assert_eq!(engine.status(cam.id).state, ConnectionState::Disabled);
    assert!(engine.frames().latest(cam.id).is_none(), "stale snapshot served after stop");
    let recs = recordings_of(&store, cam.id);
    assert!(!recs.is_empty() && recs.iter().all(|r| r.end.is_some()), "open segment not finished: {recs:?}");
    // Stopping a camera without a worker is a no-op.
    engine.stop_camera(cam.id).await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_dangling_segment_is_closed_with_zero_totals() {
    let dir = tempfile::tempdir().unwrap();
    let store = new_store();
    let cam = store
        .create_camera(&synthetic("unreadable", Schedule::default(), false, MotionConfig::default()))
        .unwrap();
    let start = Utc.with_ymd_and_hms(2026, 5, 2, 9, 0, 0).unwrap();
    let rel = segment_rel_path(cam.id, start);
    let data_path = dir.path().join(&rel);
    std::fs::create_dir_all(data_path.parent().unwrap()).unwrap();
    std::fs::write(&data_path, b"partial").unwrap();
    // A directory where the index file should be: reading it fails with a non-NotFound error.
    std::fs::create_dir(ss_recorder::segment::index_path_for(&data_path)).unwrap();
    let rec = store
        .create_recording(&NewRecording { camera_id: cam.id, path: rel.clone(), start, trigger: RecordingTrigger::Continuous })
        .unwrap();

    let engine = engine_in(&store, dir.path());
    engine.start().await.unwrap();
    engine.shutdown().await;

    let done = store.get_recording(rec.id).expect("row kept");
    assert_eq!(done.end, Some(start), "row must be closed");
    assert_eq!((done.frames, done.size_bytes), (0, 0));
    assert!(data_path.exists(), "unreadable file must be kept");
}
