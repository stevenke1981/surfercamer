use chrono::{DateTime, Duration, DurationRound, TimeZone, Utc};
use ss_core::*;

use super::SqliteStore;

fn store() -> SqliteStore {
    SqliteStore::open_in_memory().expect("in-memory store")
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 123_456_000)
        .single()
        .expect("valid timestamp")
}

fn cam_input(name: &str) -> CameraInput {
    CameraInput {
        name: name.into(),
        enabled: true,
        source: CameraSource::Rtsp {
            url: "rtsp://cam/stream".into(),
            username: Some("admin".into()),
            password: Some("secret".into()),
        },
        recording: RecordingConfig::default(),
        motion: MotionConfig::default(),
    }
}

fn new_rec(
    camera_id: CameraId,
    path: &str,
    start: DateTime<Utc>,
    trigger: RecordingTrigger,
) -> NewRecording {
    NewRecording {
        camera_id,
        path: path.into(),
        start,
        trigger,
    }
}

fn new_event(camera_id: Option<CameraId>, kind: EventKind, at_: DateTime<Utc>) -> NewEvent {
    NewEvent {
        camera_id,
        kind,
        at: at_,
        score: 0.25,
        message: "test".into(),
        recording_id: None,
    }
}

// ------------------------------------------------------------ open / schema

#[test]
fn in_memory_store_has_schema_and_pragmas() {
    let s = store();
    let conn = s.lock().unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1);
}

#[test]
fn file_store_persists_and_reopens_without_remigrating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("ss.db");
    {
        let s = SqliteStore::open(&path).unwrap();
        s.create_camera(&cam_input("Persisted")).unwrap();
        let conn = s.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
    let s = SqliteStore::open(&path).unwrap();
    let cams = s.list_cameras().unwrap();
    assert_eq!(cams.len(), 1);
    assert_eq!(cams[0].name, "Persisted");
}

#[test]
fn open_rejects_newer_schema_version() {
    let s = store();
    {
        let conn = s.lock().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let mut conn = s.conn.into_inner().unwrap();
    let err = super::migrate(&mut conn).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "got {err:?}");
}

// ------------------------------------------------------------------ cameras

#[test]
fn camera_crud_roundtrip() {
    let s = store();
    let created = s.create_camera(&cam_input("Front")).unwrap();
    assert_eq!(created.name, "Front");
    assert!(created.enabled);
    // Full source is stored (redaction is the API layer's job).
    assert_eq!(
        created.source,
        CameraSource::Rtsp {
            url: "rtsp://cam/stream".into(),
            username: Some("admin".into()),
            password: Some("secret".into()),
        }
    );
    assert_eq!(created.recording, RecordingConfig::default());
    assert_eq!(created.motion, MotionConfig::default());

    let got = s.get_camera(created.id).unwrap();
    assert_eq!(got, created);

    let mut input = cam_input("Front door");
    input.enabled = false;
    input.motion.regions.push(Region {
        x: 0.1,
        y: 0.2,
        w: 0.3,
        h: 0.4,
    });
    input.source = CameraSource::Synthetic {
        width: 640,
        height: 480,
        fps: 5,
    };
    let updated = s.update_camera(created.id, &input).unwrap();
    assert_eq!(updated.name, "Front door");
    assert!(!updated.enabled);
    assert_eq!(
        updated.source,
        CameraSource::Synthetic {
            width: 640,
            height: 480,
            fps: 5
        }
    );
    assert_eq!(updated.motion.regions, input.motion.regions);
    assert_eq!(updated.created_at, created.created_at);
    assert!(updated.updated_at >= created.updated_at);

    let all = s.list_cameras().unwrap();
    assert_eq!(all, vec![updated.clone()]);

    s.delete_camera(created.id).unwrap();
    assert!(matches!(s.get_camera(created.id), Err(Error::NotFound(_))));
    assert!(s.list_cameras().unwrap().is_empty());
}

#[test]
fn camera_missing_ids_are_not_found() {
    let s = store();
    assert!(matches!(s.get_camera(42), Err(Error::NotFound(_))));
    assert!(matches!(
        s.update_camera(42, &cam_input("x")),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(s.delete_camera(42), Err(Error::NotFound(_))));
}

#[test]
fn camera_name_must_be_unique() {
    let s = store();
    let a = s.create_camera(&cam_input("Garage")).unwrap();
    assert!(matches!(
        s.create_camera(&cam_input("Garage")),
        Err(Error::Conflict(_))
    ));
    let b = s.create_camera(&cam_input("Yard")).unwrap();
    // Renaming onto an existing name conflicts too.
    assert!(matches!(
        s.update_camera(b.id, &cam_input("Garage")),
        Err(Error::Conflict(_))
    ));
    // Keeping its own name is fine.
    s.update_camera(a.id, &cam_input("Garage")).unwrap();
}

#[test]
fn list_cameras_is_ordered_by_id() {
    let s = store();
    let a = s.create_camera(&cam_input("b")).unwrap();
    let b = s.create_camera(&cam_input("a")).unwrap();
    let ids: Vec<_> = s.list_cameras().unwrap().iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![a.id, b.id]);
}

// --------------------------------------------------------------- recordings

#[test]
fn recording_create_and_finish() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    let rec = s
        .create_recording(&new_rec(
            cam.id,
            "cam1/a.mjpg",
            at(0),
            RecordingTrigger::Motion,
        ))
        .unwrap();
    assert_eq!(rec.camera_id, cam.id);
    assert_eq!(rec.path, "cam1/a.mjpg");
    assert_eq!(rec.start, at(0));
    assert_eq!(rec.end, None);
    assert_eq!(rec.frames, 0);
    assert_eq!(rec.size_bytes, 0);
    assert_eq!(rec.trigger, RecordingTrigger::Motion);
    assert!(!rec.locked);

    let done = s.finish_recording(rec.id, at(60), 600, 123_456).unwrap();
    assert_eq!(done.end, Some(at(60)));
    assert_eq!(done.frames, 600);
    assert_eq!(done.size_bytes, 123_456);
    assert_eq!(s.get_recording(rec.id).unwrap(), done);
}

#[test]
fn recording_duplicate_path_conflicts_and_bad_camera_is_not_found() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    s.create_recording(&new_rec(cam.id, "p", at(0), RecordingTrigger::Continuous))
        .unwrap();
    assert!(matches!(
        s.create_recording(&new_rec(cam.id, "p", at(1), RecordingTrigger::Continuous)),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        s.create_recording(&new_rec(9999, "q", at(1), RecordingTrigger::Continuous)),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn recording_missing_ids_are_not_found() {
    let s = store();
    assert!(matches!(s.get_recording(1), Err(Error::NotFound(_))));
    assert!(matches!(
        s.finish_recording(1, at(0), 0, 0),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        s.set_recording_locked(1, true),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(s.delete_recording(1), Err(Error::NotFound(_))));
}

#[test]
fn list_recordings_filters_and_ordering() {
    let s = store();
    let cam1 = s.create_camera(&cam_input("c1")).unwrap();
    let cam2 = s.create_camera(&cam_input("c2")).unwrap();

    // cam1: [0,10) continuous, [20,30) motion, open from 40
    let r1 = s
        .create_recording(&new_rec(cam1.id, "r1", at(0), RecordingTrigger::Continuous))
        .unwrap();
    s.finish_recording(r1.id, at(10), 100, 1000).unwrap();
    let r2 = s
        .create_recording(&new_rec(cam1.id, "r2", at(20), RecordingTrigger::Motion))
        .unwrap();
    s.finish_recording(r2.id, at(30), 100, 2000).unwrap();
    let r3 = s
        .create_recording(&new_rec(cam1.id, "r3", at(40), RecordingTrigger::Manual))
        .unwrap();
    // cam2: one continuous recording
    let r4 = s
        .create_recording(&new_rec(cam2.id, "r4", at(5), RecordingTrigger::Continuous))
        .unwrap();

    // Unfiltered: newest start first, all cameras.
    let all = s.list_recordings(&RecordingQuery::default()).unwrap();
    let ids: Vec<_> = all.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![r3.id, r2.id, r4.id, r1.id]);

    // Camera filter.
    let q = RecordingQuery {
        camera_id: Some(cam1.id),
        ..Default::default()
    };
    let ids: Vec<_> = s
        .list_recordings(&q)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![r3.id, r2.id, r1.id]);

    // Trigger filter.
    let q = RecordingQuery {
        trigger: Some(RecordingTrigger::Motion),
        ..Default::default()
    };
    let ids: Vec<_> = s
        .list_recordings(&q)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![r2.id]);

    // Locked filter.
    s.set_recording_locked(r1.id, true).unwrap();
    let q = RecordingQuery {
        locked: Some(true),
        ..Default::default()
    };
    let ids: Vec<_> = s
        .list_recordings(&q)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![r1.id]);
    let q = RecordingQuery {
        locked: Some(false),
        ..Default::default()
    };
    assert_eq!(s.list_recordings(&q).unwrap().len(), 3);

    // Limit and offset.
    let q = RecordingQuery {
        limit: Some(2),
        offset: Some(1),
        ..Default::default()
    };
    let ids: Vec<_> = s
        .list_recordings(&q)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![r2.id, r4.id]);

    // Limit above the maximum is clamped, not an error.
    let q = RecordingQuery {
        limit: Some(50_000),
        ..Default::default()
    };
    assert_eq!(s.list_recordings(&q).unwrap().len(), 4);
}

#[test]
fn list_recordings_overlap_window_semantics() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    // A: [0, 10]   B: [20, open)
    let a = s
        .create_recording(&new_rec(cam.id, "a", at(0), RecordingTrigger::Continuous))
        .unwrap();
    s.finish_recording(a.id, at(10), 1, 1).unwrap();
    let b = s
        .create_recording(&new_rec(cam.id, "b", at(20), RecordingTrigger::Continuous))
        .unwrap();

    let ids = |from: Option<i64>, to: Option<i64>| {
        let q = RecordingQuery {
            from: from.map(at),
            to: to.map(at),
            ..Default::default()
        };
        s.list_recordings(&q)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
    };

    // start < to: a recording starting exactly at `to` is excluded.
    assert_eq!(ids(None, Some(20)), vec![a.id]);
    assert_eq!(ids(None, Some(21)), vec![b.id, a.id]);
    // end >= from: a recording ending exactly at `from` is included.
    assert_eq!(ids(Some(10), Some(11)), vec![a.id]);
    assert_eq!(ids(Some(11), Some(15)), Vec::<i64>::new());
    // Open recording overlaps any window that starts after its start.
    assert_eq!(ids(Some(1_000), None), vec![b.id]);
    assert_eq!(ids(Some(1_000), Some(2_000)), vec![b.id]);
}

#[test]
fn set_locked_and_delete_recording() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    let r = s
        .create_recording(&new_rec(cam.id, "x", at(0), RecordingTrigger::Manual))
        .unwrap();
    let locked = s.set_recording_locked(r.id, true).unwrap();
    assert!(locked.locked);
    let unlocked = s.set_recording_locked(r.id, false).unwrap();
    assert!(!unlocked.locked);
    s.delete_recording(r.id).unwrap();
    assert!(matches!(s.get_recording(r.id), Err(Error::NotFound(_))));
}

#[test]
fn recording_usage_sums_sizes() {
    let s = store();
    assert_eq!(s.recording_usage(None).unwrap(), 0);
    let c1 = s.create_camera(&cam_input("c1")).unwrap();
    let c2 = s.create_camera(&cam_input("c2")).unwrap();
    assert_eq!(s.recording_usage(Some(c1.id)).unwrap(), 0);

    let a = s
        .create_recording(&new_rec(c1.id, "a", at(0), RecordingTrigger::Continuous))
        .unwrap();
    s.finish_recording(a.id, at(1), 1, 100).unwrap();
    let b = s
        .create_recording(&new_rec(c1.id, "b", at(2), RecordingTrigger::Continuous))
        .unwrap();
    s.finish_recording(b.id, at(3), 1, 250).unwrap();
    let c = s
        .create_recording(&new_rec(c2.id, "c", at(0), RecordingTrigger::Continuous))
        .unwrap();
    s.finish_recording(c.id, at(1), 1, 1000).unwrap();

    assert_eq!(s.recording_usage(Some(c1.id)).unwrap(), 350);
    assert_eq!(s.recording_usage(Some(c2.id)).unwrap(), 1000);
    assert_eq!(s.recording_usage(None).unwrap(), 1350);
}

#[test]
fn retention_candidates_are_unlocked_finished_oldest_first() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    let other = s.create_camera(&cam_input("other")).unwrap();

    let old = s
        .create_recording(&new_rec(cam.id, "old", at(0), RecordingTrigger::Continuous))
        .unwrap();
    s.finish_recording(old.id, at(5), 1, 1).unwrap();
    let locked = s
        .create_recording(&new_rec(
            cam.id,
            "locked",
            at(10),
            RecordingTrigger::Continuous,
        ))
        .unwrap();
    s.finish_recording(locked.id, at(15), 1, 1).unwrap();
    s.set_recording_locked(locked.id, true).unwrap();
    // Still being written: never a candidate.
    s.create_recording(&new_rec(
        cam.id,
        "open",
        at(20),
        RecordingTrigger::Continuous,
    ))
    .unwrap();
    let newer = s
        .create_recording(&new_rec(cam.id, "newer", at(30), RecordingTrigger::Motion))
        .unwrap();
    s.finish_recording(newer.id, at(35), 1, 1).unwrap();
    // Another camera's recording is excluded.
    let foreign = s
        .create_recording(&new_rec(
            other.id,
            "foreign",
            at(0),
            RecordingTrigger::Continuous,
        ))
        .unwrap();
    s.finish_recording(foreign.id, at(1), 1, 1).unwrap();

    let ids: Vec<_> = s
        .retention_candidates(cam.id)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![old.id, newer.id]);
}

// ------------------------------------------------------------------- events

#[test]
fn event_create_list_ack_delete() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    let other = s.create_camera(&cam_input("o")).unwrap();

    let e1 = s
        .create_event(&new_event(Some(cam.id), EventKind::Motion, at(0)))
        .unwrap();
    assert_eq!(e1.kind, EventKind::Motion);
    assert_eq!(e1.at, at(0));
    assert!((e1.score - 0.25).abs() < f32::EPSILON);
    assert!(!e1.acknowledged);
    assert_eq!(e1.recording_id, None);

    let e2 = s
        .create_event(&new_event(Some(cam.id), EventKind::ConnectionLost, at(10)))
        .unwrap();
    let e3 = s
        .create_event(&new_event(Some(other.id), EventKind::Motion, at(20)))
        .unwrap();
    let e4 = s
        .create_event(&new_event(None, EventKind::StorageFull, at(30)))
        .unwrap();
    assert_eq!(e4.camera_id, None);

    // Default ordering: newest first.
    let ids: Vec<_> = s
        .list_events(&EventQuery::default())
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(ids, vec![e4.id, e3.id, e2.id, e1.id]);

    // Camera and kind filters.
    let q = EventQuery {
        camera_id: Some(cam.id),
        ..Default::default()
    };
    assert_eq!(s.list_events(&q).unwrap().len(), 2);
    let q = EventQuery {
        kind: Some(EventKind::Motion),
        ..Default::default()
    };
    assert_eq!(s.list_events(&q).unwrap().len(), 2);

    // Half-open time window [from, to).
    let q = EventQuery {
        from: Some(at(10)),
        to: Some(at(30)),
        ..Default::default()
    };
    let ids: Vec<_> = s.list_events(&q).unwrap().iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![e3.id, e2.id]);

    // Acknowledged filter and acknowledge.
    let acked = s.acknowledge_event(e3.id).unwrap();
    assert!(acked.acknowledged);
    let q = EventQuery {
        acknowledged: Some(true),
        ..Default::default()
    };
    let ids: Vec<_> = s.list_events(&q).unwrap().iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![e3.id]);
    let q = EventQuery {
        acknowledged: Some(false),
        ..Default::default()
    };
    assert_eq!(s.list_events(&q).unwrap().len(), 3);

    // Limit / offset.
    let q = EventQuery {
        limit: Some(1),
        offset: Some(1),
        ..Default::default()
    };
    let ids: Vec<_> = s.list_events(&q).unwrap().iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![e3.id]);

    assert!(matches!(s.acknowledge_event(9999), Err(Error::NotFound(_))));

    // Delete everything strictly before at(15): e1 (0) and e2 (10).
    let removed = s.delete_events_before(at(15)).unwrap();
    assert_eq!(removed, 2);
    let remaining = s.list_events(&EventQuery::default()).unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(s.delete_events_before(at(15)).unwrap(), 0);
}

#[test]
fn event_rejects_non_finite_score_and_unknown_camera() {
    let s = store();
    let mut ev = new_event(None, EventKind::Motion, at(0));
    ev.score = f32::NAN;
    assert!(matches!(s.create_event(&ev), Err(Error::Invalid(_))));
    let ev = new_event(Some(777), EventKind::Motion, at(0));
    assert!(matches!(s.create_event(&ev), Err(Error::NotFound(_))));
}

#[test]
fn events_keep_row_but_lose_recording_when_recording_deleted() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    let rec = s
        .create_recording(&new_rec(cam.id, "r", at(0), RecordingTrigger::Motion))
        .unwrap();
    let mut ev = new_event(Some(cam.id), EventKind::Motion, at(1));
    ev.recording_id = Some(rec.id);
    let ev = s.create_event(&ev).unwrap();
    assert_eq!(ev.recording_id, Some(rec.id));

    s.delete_recording(rec.id).unwrap();
    let ev = s.list_events(&EventQuery::default()).unwrap().remove(0);
    assert_eq!(ev.recording_id, None);
}

// -------------------------------------------------------------------- users

#[test]
fn user_crud_and_counts() {
    let s = store();
    assert_eq!(s.count_users().unwrap(), 0);

    let admin = s.create_user("admin", "hash-a", Role::Admin).unwrap();
    assert_eq!(admin.username, "admin");
    assert_eq!(admin.role, Role::Admin);
    assert_eq!(admin.password_hash, "hash-a");
    let viewer = s.create_user("viewer", "hash-v", Role::Viewer).unwrap();
    assert_eq!(s.count_users().unwrap(), 2);

    assert_eq!(s.get_user(viewer.id).unwrap(), viewer);
    assert_eq!(s.get_user_by_name("viewer").unwrap(), viewer);
    assert_eq!(s.list_users().unwrap(), vec![admin.clone(), viewer.clone()]);

    let promoted = s
        .update_user(viewer.id, Some("new-hash"), Some(Role::Manager))
        .unwrap();
    assert_eq!(promoted.password_hash, "new-hash");
    assert_eq!(promoted.role, Role::Manager);

    // None fields are left unchanged.
    let same = s.update_user(viewer.id, None, None).unwrap();
    assert_eq!(same, promoted);
    let role_only = s.update_user(viewer.id, None, Some(Role::Viewer)).unwrap();
    assert_eq!(role_only.password_hash, "new-hash");
    assert_eq!(role_only.role, Role::Viewer);

    s.delete_user(viewer.id).unwrap();
    assert_eq!(s.count_users().unwrap(), 1);
    assert!(matches!(s.get_user(viewer.id), Err(Error::NotFound(_))));
    assert!(matches!(
        s.get_user_by_name("viewer"),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        s.update_user(viewer.id, None, None),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(s.delete_user(viewer.id), Err(Error::NotFound(_))));
}

#[test]
fn username_is_case_insensitive_and_unique() {
    let s = store();
    s.create_user("Alice", "h", Role::Viewer).unwrap();
    assert!(matches!(
        s.create_user("alice", "h", Role::Viewer),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        s.create_user("ALICE", "h", Role::Admin),
        Err(Error::Conflict(_))
    ));
    let found = s.get_user_by_name("aLiCe").unwrap();
    assert_eq!(found.username, "Alice");
}

// ----------------------------------------------------------------- sessions

fn session(token: &str, user_id: UserId, expires_in: Duration) -> Session {
    // Store keeps microseconds; align so round-trip equality holds.
    let now = Utc::now()
        .duration_trunc(Duration::microseconds(1))
        .expect("trunc");
    Session {
        token: token.into(),
        user_id,
        created_at: now,
        expires_at: now + expires_in,
    }
}

#[test]
fn session_create_get_delete() {
    let s = store();
    let u = s.create_user("u", "h", Role::Viewer).unwrap();
    let sess = session("tok", u.id, Duration::hours(1));
    s.create_session(&sess).unwrap();

    let got = s.get_session("tok").unwrap();
    assert_eq!(got.user_id, u.id);
    assert_eq!(got.expires_at, sess.expires_at);

    // Duplicate token conflicts.
    assert!(matches!(
        s.create_session(&session("tok", u.id, Duration::hours(1))),
        Err(Error::Conflict(_))
    ));
    // Session for a missing user is rejected.
    assert!(matches!(
        s.create_session(&session("other", 9999, Duration::hours(1))),
        Err(Error::NotFound(_))
    ));

    s.delete_session("tok").unwrap();
    assert!(matches!(s.get_session("tok"), Err(Error::NotFound(_))));
    assert!(matches!(s.delete_session("tok"), Err(Error::NotFound(_))));
}

#[test]
fn expired_session_is_not_found_and_purge_removes_it() {
    let s = store();
    let u = s.create_user("u", "h", Role::Viewer).unwrap();
    s.create_session(&session("live", u.id, Duration::hours(1)))
        .unwrap();
    s.create_session(&session("dead", u.id, Duration::seconds(-60)))
        .unwrap();
    s.create_session(&session("dead2", u.id, Duration::seconds(-120)))
        .unwrap();

    assert!(s.get_session("live").is_ok());
    assert!(matches!(s.get_session("dead"), Err(Error::NotFound(_))));

    let removed = s.purge_expired_sessions(Utc::now()).unwrap();
    assert_eq!(removed, 2);
    assert!(s.get_session("live").is_ok());
    assert_eq!(s.purge_expired_sessions(Utc::now()).unwrap(), 0);
}

#[test]
fn deleting_user_cascades_to_sessions() {
    let s = store();
    let u = s.create_user("u", "h", Role::Viewer).unwrap();
    s.create_session(&session("t", u.id, Duration::hours(1)))
        .unwrap();
    s.delete_user(u.id).unwrap();
    assert!(matches!(s.get_session("t"), Err(Error::NotFound(_))));
}

// ----------------------------------------------------------------- settings

#[test]
fn settings_default_then_roundtrip() {
    let s = store();
    assert_eq!(s.get_settings().unwrap(), Settings::default());

    let custom = Settings {
        min_free_bytes: 5 << 30,
        session_ttl_hours: 2,
        retention_interval_seconds: 60,
    };
    s.put_settings(&custom).unwrap();
    assert_eq!(s.get_settings().unwrap(), custom);

    let again = Settings {
        session_ttl_hours: 48,
        ..custom.clone()
    };
    s.put_settings(&again).unwrap();
    assert_eq!(s.get_settings().unwrap(), again);
}

// ------------------------------------------------------------------ cascade

#[test]
fn deleting_camera_cascades_to_recordings_and_events() {
    let s = store();
    let cam = s.create_camera(&cam_input("doomed")).unwrap();
    let keep = s.create_camera(&cam_input("kept")).unwrap();

    let rec = s
        .create_recording(&new_rec(
            cam.id,
            "doomed/r",
            at(0),
            RecordingTrigger::Motion,
        ))
        .unwrap();
    s.create_recording(&new_rec(keep.id, "kept/r", at(0), RecordingTrigger::Motion))
        .unwrap();
    s.create_event(&new_event(Some(cam.id), EventKind::Motion, at(1)))
        .unwrap();
    s.create_event(&new_event(Some(keep.id), EventKind::Motion, at(1)))
        .unwrap();
    // Event with no camera survives.
    s.create_event(&new_event(None, EventKind::StorageFull, at(2)))
        .unwrap();

    s.delete_camera(cam.id).unwrap();

    assert!(matches!(s.get_recording(rec.id), Err(Error::NotFound(_))));
    let recs = s.list_recordings(&RecordingQuery::default()).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].camera_id, keep.id);

    let evs = s.list_events(&EventQuery::default()).unwrap();
    assert_eq!(evs.len(), 2);
    assert!(evs.iter().all(|e| e.camera_id != Some(cam.id)));
}

#[test]
fn delete_sessions_for_user_keeps_excepted_token() {
    let s = store();
    let u = s.create_user("u", "h", Role::Viewer).unwrap();
    let other = s.create_user("o", "h", Role::Viewer).unwrap();
    for t in ["a", "b", "c"] {
        s.create_session(&session(t, u.id, Duration::hours(1)))
            .unwrap();
    }
    s.create_session(&session("x", other.id, Duration::hours(1)))
        .unwrap();
    assert_eq!(s.delete_sessions_for_user(u.id, Some("b")).unwrap(), 2);
    assert!(s.get_session("b").is_ok());
    assert!(s.get_session("x").is_ok());
    assert_eq!(s.delete_sessions_for_user(u.id, None).unwrap(), 1);
    assert!(matches!(s.get_session("b"), Err(Error::NotFound(_))));
}

#[test]
fn set_event_recording_links_event() {
    let s = store();
    let cam = s.create_camera(&cam_input("c")).unwrap();
    let rec = s
        .create_recording(&new_rec(
            cam.id,
            "cam1/r.mjpg",
            Utc::now(),
            RecordingTrigger::Motion,
        ))
        .unwrap();
    let ev = s
        .create_event(&new_event(Some(cam.id), EventKind::Motion, Utc::now()))
        .unwrap();
    s.set_event_recording(ev.id, rec.id).unwrap();
    let listed = s.list_events(&EventQuery::default()).unwrap();
    assert_eq!(listed[0].recording_id, Some(rec.id));
    assert!(matches!(
        s.set_event_recording(9999, rec.id),
        Err(Error::NotFound(_))
    ));
}
