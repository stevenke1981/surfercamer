//! HTTP API integration tests: router + in-memory SQLite + temp storage dir.
//! The engine is constructed but never started.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use bytes::Bytes;
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use ss_core::{Frame, NewEvent, NewRecording, RecordingTrigger, Role, Store};
use ss_db::SqliteStore;
use ss_engine::{Engine, EngineConfig};
use ss_server::{auth, router, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

struct Harness {
    app: Router,
    state: AppState,
    _dir: TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().unwrap());
    let root = dir.path().join("recordings");
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::new(store.clone(), EngineConfig { storage_root: root });
    auth::bootstrap_admin(store.as_ref(), Some("adminpass")).unwrap();
    for (name, role) in [("viewer", Role::Viewer), ("manager", Role::Manager)] {
        let hash = auth::hash_password("password123").unwrap();
        store.create_user(name, &hash, role).unwrap();
    }
    let state = AppState::new(engine, store);
    Harness { app: router(state.clone()), state, _dir: dir }
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not json ({e}): {:?}", self.body))
    }
}

async fn call(app: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> Resp {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Resp { status, headers, body }
}

async fn login(h: &Harness, user: &str, pw: &str) -> String {
    let r = call(&h.app, "POST", "/api/v1/auth/login", None, Some(json!({"username": user, "password": pw}))).await;
    assert_eq!(r.status, StatusCode::OK, "login {user}");
    r.json()["token"].as_str().unwrap().to_string()
}

fn camera_body(name: &str) -> Value {
    json!({"name": name, "source": {"type": "synthetic", "width": 160, "height": 120, "fps": 5}})
}

fn add_camera(h: &Harness, name: &str) -> i64 {
    let input: ss_core::CameraInput = serde_json::from_value(camera_body(name)).unwrap();
    h.state.store.create_camera(&input).unwrap().id
}

#[tokio::test]
async fn health_is_public() {
    let h = harness();
    let r = call(&h.app, "GET", "/health", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(&r.body[..], b"ok");
}

#[tokio::test]
async fn api_requires_token() {
    let h = harness();
    let r = call(&h.app, "GET", "/api/v1/auth/me", None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"], "unauthorized");
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some("deadbeef"), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_success_sets_cookie_and_me_works() {
    let h = harness();
    let r = call(&h.app, "POST", "/api/v1/auth/login", None, Some(json!({"username": "admin", "password": "adminpass"}))).await;
    assert_eq!(r.status, StatusCode::OK);
    let cookie = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(cookie.starts_with("ss_session=") && cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
    let token = r.json()["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);

    let me = call(&h.app, "GET", "/api/v1/auth/me", Some(&token), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["username"], "admin");
    assert!(me.json().get("password_hash").is_none());

    // Cookie and query-string tokens are accepted too.
    let by_query = call(&h.app, "GET", &format!("/api/v1/auth/me?token={token}"), None, None).await;
    assert_eq!(by_query.status, StatusCode::OK);
}

#[tokio::test]
async fn login_failure_and_rate_limit() {
    let h = harness();
    for _ in 0..5 {
        let r = call(&h.app, "POST", "/api/v1/auth/login", None, Some(json!({"username": "viewer", "password": "wrong-pass"}))).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    // Blocked now, even with the right password.
    let r = call(&h.app, "POST", "/api/v1/auth/login", None, Some(json!({"username": "viewer", "password": "password123"}))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json()["error"], "rate_limited");
}

#[tokio::test]
async fn logout_is_idempotent() {
    let h = harness();
    let token = login(&h, "viewer", "password123").await;
    let r = call(&h.app, "POST", "/api/v1/auth/logout", Some(&token), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some(&token), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn viewer_cannot_modify_cameras_and_invalid_input_is_400() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let r = call(&h.app, "POST", "/api/v1/cameras", Some(&viewer), Some(camera_body("cam"))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let manager = login(&h, "manager", "password123").await;
    let r = call(&h.app, "POST", "/api/v1/cameras", Some(&manager), Some(camera_body("  "))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = call(&h.app, "POST", "/api/v1/cameras", Some(&manager), Some(json!({"name": "x", "source": {"type": "bogus"}}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn camera_password_is_redacted_and_mask_keeps_stored_value() {
    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let body = json!({"name": "mj", "source": {"type": "mjpeg", "url": "http://127.0.0.1:9/x", "username": "u", "password": "hunter22"}});
    let r = call(&h.app, "POST", "/api/v1/cameras", Some(&manager), Some(body)).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.json()["id"].as_i64().unwrap();
    assert_eq!(r.json()["source"]["password"], "********");
    assert!(!String::from_utf8_lossy(&r.body).contains("hunter22"));

    let put = json!({"name": "mj2", "source": {"type": "mjpeg", "url": "http://127.0.0.1:9/x", "username": "u", "password": "********"}});
    let r = call(&h.app, "PUT", &format!("/api/v1/cameras/{id}"), Some(&manager), Some(put)).await;
    assert_eq!(r.status, StatusCode::OK);
    match h.state.store.get_camera(id).unwrap().source {
        ss_core::CameraSource::Mjpeg { password, .. } => assert_eq!(password.as_deref(), Some("hunter22")),
        other => panic!("unexpected source {other:?}"),
    }
}

#[tokio::test]
async fn recordings_list_and_delete_guards() {
    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let cam = add_camera(&h, "rec-cam");
    let open = h
        .state
        .store
        .create_recording(&NewRecording { camera_id: cam, path: "cam1/open.mjpg".into(), start: Utc::now(), trigger: RecordingTrigger::Manual })
        .unwrap();

    let list = call(&h.app, "GET", &format!("/api/v1/recordings?camera_id={cam}"), Some(&manager), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json().as_array().unwrap().len(), 1);

    // Still open -> 409.
    let r = call(&h.app, "DELETE", &format!("/api/v1/recordings/{}", open.id), Some(&manager), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Finished but locked -> 409.
    let done = h
        .state
        .store
        .create_recording(&NewRecording { camera_id: cam, path: "cam1/done.mjpg".into(), start: Utc::now(), trigger: RecordingTrigger::Continuous })
        .unwrap();
    h.state.store.finish_recording(done.id, Utc::now(), 3, 300).unwrap();
    let lock = call(&h.app, "POST", &format!("/api/v1/recordings/{}/lock", done.id), Some(&manager), None).await;
    assert_eq!(lock.status, StatusCode::OK);
    assert_eq!(lock.json()["locked"], true);
    let r = call(&h.app, "DELETE", &format!("/api/v1/recordings/{}", done.id), Some(&manager), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Unlocked and finished (files absent) -> 204, row gone.
    call(&h.app, "POST", &format!("/api/v1/recordings/{}/unlock", done.id), Some(&manager), None).await;
    let r = call(&h.app, "DELETE", &format!("/api/v1/recordings/{}", done.id), Some(&manager), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = call(&h.app, "GET", &format!("/api/v1/recordings/{}", done.id), Some(&manager), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn events_list_and_ack() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "ev-cam");
    let ev = h
        .state
        .engine
        .emit_event(NewEvent { camera_id: Some(cam), kind: ss_core::EventKind::Motion, at: Utc::now(), score: 0.5, message: "Motion detected".into(), recording_id: None })
        .unwrap();

    let list = call(&h.app, "GET", "/api/v1/events?acknowledged=false", Some(&viewer), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.json().as_array().unwrap().iter().any(|e| e["id"] == ev.id));

    let ack = call(&h.app, "POST", &format!("/api/v1/events/{}/ack", ev.id), Some(&viewer), None).await;
    assert_eq!(ack.status, StatusCode::OK);
    assert_eq!(ack.json()["acknowledged"], true);

    let bad = call(&h.app, "GET", "/api/v1/events?kind=nonsense", Some(&viewer), None).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn users_are_admin_only_and_last_admin_is_protected() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let admin = login(&h, "admin", "adminpass").await;

    let r = call(&h.app, "GET", "/api/v1/users", Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let r = call(&h.app, "GET", "/api/v1/users", Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&r.body).contains("argon2"));

    let short = json!({"username": "newbie", "password": "short", "role": "viewer"});
    let r = call(&h.app, "POST", "/api/v1/users", Some(&admin), Some(short)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let ok = json!({"username": "newbie", "password": "longenough", "role": "viewer"});
    let r = call(&h.app, "POST", "/api/v1/users", Some(&admin), Some(ok)).await;
    assert_eq!(r.status, StatusCode::CREATED);

    let admin_id = h.state.store.get_user_by_name("admin").unwrap().id;
    let r = call(&h.app, "PUT", &format!("/api/v1/users/{admin_id}"), Some(&admin), Some(json!({"role": "viewer"}))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = call(&h.app, "DELETE", &format!("/api/v1/users/{admin_id}"), Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn snapshot_503_then_200_after_publish() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "snap-cam");
    let r = call(&h.app, "GET", &format!("/api/v1/cameras/{cam}/snapshot"), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);

    h.state.engine.frames().publish(Frame {
        camera_id: cam,
        ts: Utc::now(),
        jpeg: Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]),
        width: 1,
        height: 1,
    });
    let r = call(&h.app, "GET", &format!("/api/v1/cameras/{cam}/snapshot"), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(&r.body[..], &[0xFF, 0xD8, 0xFF, 0xD9]);
}

#[tokio::test]
async fn settings_get_and_put() {
    let h = harness();
    let admin = login(&h, "admin", "adminpass").await;
    let viewer = login(&h, "viewer", "password123").await;

    let r = call(&h.app, "GET", "/api/v1/settings", Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["session_ttl_hours"], 24);

    let r = call(&h.app, "GET", "/api/v1/settings", Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let bad = json!({"min_free_bytes": 0, "session_ttl_hours": 0, "retention_interval_seconds": 300});
    let r = call(&h.app, "PUT", "/api/v1/settings", Some(&admin), Some(bad)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let good = json!({"min_free_bytes": 5, "session_ttl_hours": 2, "retention_interval_seconds": 60});
    let r = call(&h.app, "PUT", "/api/v1/settings", Some(&admin), Some(good)).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = call(&h.app, "GET", "/api/v1/settings", Some(&admin), None).await;
    assert_eq!(r.json()["min_free_bytes"], 5);
}

#[tokio::test]
async fn create_camera_and_list_with_status() {
    // Goes through Engine::reload_camera / status (engine must be implemented).
    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let r = call(&h.app, "POST", "/api/v1/cameras", Some(&manager), Some(camera_body("front"))).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.json()["id"].as_i64().unwrap();
    assert!(r.json()["status"].is_object());

    let list = call(&h.app, "GET", "/api/v1/cameras", Some(&manager), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.json().as_array().unwrap().iter().any(|c| c["id"] == id));

    let r = call(&h.app, "DELETE", &format!("/api/v1/cameras/{id}"), Some(&manager), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn system_info_reports_version() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let r = call(&h.app, "GET", "/api/v1/system/info", Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json()["version"].is_string());
    assert!(r.json()["storage"]["root"].is_string());
    assert!(r.json()["cameras"]["total"].is_number());
}

#[tokio::test]
async fn timeline_rejects_bad_date() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "tl");
    let r = call(&h.app, "GET", &format!("/api/v1/timeline?camera_id={cam}&date=2026-13-40"), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = call(&h.app, "GET", &format!("/api/v1/timeline?camera_id={cam}&date=2026-03-04"), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json()["recordings"].is_array() && r.json()["events"].is_array());
}

#[tokio::test]
async fn web_ui_is_served() {
    let h = harness();
    let r = call(&h.app, "GET", "/", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
    let r = call(&h.app, "GET", "/app.js", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn playback_frame_and_download_serve_segment_data() {
    use ss_recorder::segment::SegmentWriter;

    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "pb-cam");
    let root = h.state.engine.storage_root().to_path_buf();
    let start = Utc::now();
    let rel = format!("cam{cam}/test/seg.mjpg");
    let mut w = SegmentWriter::create(&root, &rel, start).unwrap();
    for (i, ms) in [0i64, 100, 200].iter().enumerate() {
        let jpeg = vec![0xFF, 0xD8, i as u8, 0xFF, 0xD9];
        w.write_frame(start + chrono::Duration::milliseconds(*ms), &jpeg).unwrap();
    }
    let summary = w.finish().unwrap();
    let rec = h
        .state
        .store
        .create_recording(&NewRecording { camera_id: cam, path: rel.clone(), start, trigger: RecordingTrigger::Manual })
        .unwrap();
    h.state.store.finish_recording(rec.id, Utc::now(), summary.frames, summary.size_bytes).unwrap();

    // Playback at 16x: three MJPEG parts.
    let r = call(&h.app, "GET", &format!("/api/v1/recordings/{}/stream?speed=16", rec.id), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_TYPE].to_str().unwrap().contains("boundary=ssframe"));
    let text = String::from_utf8_lossy(&r.body);
    assert_eq!(text.matches("--ssframe\r\nContent-Type: image/jpeg").count(), 3);

    // Single frame nearest at/before 150 ms is frame 1.
    let r = call(&h.app, "GET", &format!("/api/v1/recordings/{}/frame?offset_ms=150", rec.id), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(&r.body[..], &[0xFF, 0xD8, 1, 0xFF, 0xD9]);

    // Download returns the raw .mjpg bytes as an attachment.
    let r = call(&h.app, "GET", &format!("/api/v1/recordings/{}/download", rec.id), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment"));
    assert_eq!(r.body.len() as u64, std::fs::metadata(root.join(&rel)).unwrap().len());

    // Viewers cannot delete; a manager can, and the segment files go with it.
    let r = call(&h.app, "DELETE", &format!("/api/v1/recordings/{}", rec.id), Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let manager = login(&h, "manager", "password123").await;
    let r = call(&h.app, "DELETE", &format!("/api/v1/recordings/{}", rec.id), Some(&manager), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(!root.join(&rel).exists());
    assert!(!root.join(format!("cam{cam}/test/seg.idx")).exists());
}

#[tokio::test]
async fn live_stream_starts_with_latest_frame() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "live-cam");
    h.state.engine.frames().publish(Frame {
        camera_id: cam,
        ts: Utc::now(),
        jpeg: Bytes::from_static(&[0xFF, 0xD8, 9, 0xFF, 0xD9]),
        width: 1,
        height: 1,
    });
    let req = Request::builder()
        .uri(format!("/api/v1/cameras/{cam}/live"))
        .header(header::AUTHORIZATION, format!("Bearer {viewer}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("multipart/x-mixed-replace"));
    let mut body = resp.into_body();
    let chunk = body.frame().await.expect("a chunk").unwrap().into_data().unwrap();
    let text = &chunk[..];
    assert!(text.starts_with(b"--ssframe\r\nContent-Type: image/jpeg\r\nContent-Length: 5\r\n\r\n"));
    assert!(text.ends_with(b"\r\n"));
}
