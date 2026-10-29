//! HTTP API integration tests: router + in-memory SQLite + temp storage dir.
//! The engine is constructed but never started.

use std::sync::Arc;
use std::time::Duration;

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
    harness_with(None)
}

/// `recheck` overrides how often open streams re-check their session.
fn harness_with(recheck: Option<Duration>) -> Harness {
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
    let mut state = AppState::new(engine, store);
    if let Some(period) = recheck {
        state.session_recheck = period;
    }
    Harness {
        app: router(state.clone()),
        state,
        _dir: dir,
    }
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not json ({e}): {:?}", self.body))
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Resp {
    let bearer = token.map(|t| format!("Bearer {t}"));
    let headers: Vec<(&str, &str)> = match bearer.as_deref() {
        Some(v) => vec![(header::AUTHORIZATION.as_str(), v)],
        None => vec![],
    };
    send(app, method, uri, &headers, body).await
}

/// Request with arbitrary headers (cookie auth, CSRF header, ...).
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Resp {
    let mut b = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        b = b.header(*name, *value);
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
    Resp {
        status,
        headers,
        body,
    }
}

async fn login(h: &Harness, user: &str, pw: &str) -> String {
    let r = call(
        &h.app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": user, "password": pw})),
    )
    .await;
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
    let r = call(
        &h.app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "admin", "password": "adminpass"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let cookie = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(
        cookie.starts_with("ss_session=")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Lax")
    );
    let token = r.json()["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);

    let me = call(&h.app, "GET", "/api/v1/auth/me", Some(&token), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["username"], "admin");
    assert!(me.json().get("password_hash").is_none());

    // Query-string tokens are ignored outside the streaming GET routes.
    let by_query = call(
        &h.app,
        "GET",
        &format!("/api/v1/auth/me?token={token}"),
        None,
        None,
    )
    .await;
    assert_eq!(by_query.status, StatusCode::UNAUTHORIZED);
    let cookie = format!("ss_session={token}");
    let me_cookie = send(
        &h.app,
        "GET",
        "/api/v1/auth/me",
        &[("cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(me_cookie.status, StatusCode::OK);
}

#[tokio::test]
async fn login_failure_and_rate_limit() {
    let h = harness();
    for _ in 0..5 {
        let r = call(
            &h.app,
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"username": "viewer", "password": "wrong-pass"})),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    // Blocked now, even with the right password.
    let r = call(
        &h.app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "viewer", "password": "password123"})),
    )
    .await;
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
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&viewer),
        Some(camera_body("cam")),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let manager = login(&h, "manager", "password123").await;
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&manager),
        Some(camera_body("  ")),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&manager),
        Some(json!({"name": "x", "source": {"type": "bogus"}})),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn camera_password_is_redacted_and_mask_keeps_stored_value() {
    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let body = json!({"name": "mj", "source": {"type": "mjpeg", "url": "http://127.0.0.1:9/x", "username": "u", "password": "hunter22"}});
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&manager),
        Some(body),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.json()["id"].as_i64().unwrap();
    assert_eq!(r.json()["source"]["password"], "********");
    assert!(!String::from_utf8_lossy(&r.body).contains("hunter22"));

    let put = json!({"name": "mj2", "source": {"type": "mjpeg", "url": "http://127.0.0.1:9/x", "username": "u", "password": "********"}});
    let r = call(
        &h.app,
        "PUT",
        &format!("/api/v1/cameras/{id}"),
        Some(&manager),
        Some(put),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    match h.state.store.get_camera(id).unwrap().source {
        ss_core::CameraSource::Mjpeg { password, .. } => {
            assert_eq!(password.as_deref(), Some("hunter22"))
        }
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
        .create_recording(&NewRecording {
            camera_id: cam,
            path: "cam1/open.mjpg".into(),
            start: Utc::now(),
            trigger: RecordingTrigger::Manual,
        })
        .unwrap();

    let list = call(
        &h.app,
        "GET",
        &format!("/api/v1/recordings?camera_id={cam}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json().as_array().unwrap().len(), 1);

    // Still open -> 409.
    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/recordings/{}", open.id),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Finished but locked -> 409.
    let done = h
        .state
        .store
        .create_recording(&NewRecording {
            camera_id: cam,
            path: "cam1/done.mjpg".into(),
            start: Utc::now(),
            trigger: RecordingTrigger::Continuous,
        })
        .unwrap();
    h.state
        .store
        .finish_recording(done.id, Utc::now(), 3, 300)
        .unwrap();
    let lock = call(
        &h.app,
        "POST",
        &format!("/api/v1/recordings/{}/lock", done.id),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(lock.status, StatusCode::OK);
    assert_eq!(lock.json()["locked"], true);
    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/recordings/{}", done.id),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Unlocked and finished (files absent) -> 204, row gone.
    call(
        &h.app,
        "POST",
        &format!("/api/v1/recordings/{}/unlock", done.id),
        Some(&manager),
        None,
    )
    .await;
    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/recordings/{}", done.id),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/recordings/{}", done.id),
        Some(&manager),
        None,
    )
    .await;
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
        .emit_event(NewEvent {
            camera_id: Some(cam),
            kind: ss_core::EventKind::Motion,
            at: Utc::now(),
            score: 0.5,
            message: "Motion detected".into(),
            recording_id: None,
        })
        .unwrap();

    let list = call(
        &h.app,
        "GET",
        "/api/v1/events?acknowledged=false",
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list
        .json()
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == ev.id));

    let ack = call(
        &h.app,
        "POST",
        &format!("/api/v1/events/{}/ack", ev.id),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(ack.status, StatusCode::OK);
    assert_eq!(ack.json()["acknowledged"], true);

    let bad = call(
        &h.app,
        "GET",
        "/api/v1/events?kind=nonsense",
        Some(&viewer),
        None,
    )
    .await;
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
    let r = call(
        &h.app,
        "PUT",
        &format!("/api/v1/users/{admin_id}"),
        Some(&admin),
        Some(json!({"role": "viewer"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/users/{admin_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn snapshot_503_then_200_after_publish() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "snap-cam");
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/cameras/{cam}/snapshot"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);

    h.state.engine.frames().publish(Frame {
        camera_id: cam,
        ts: Utc::now(),
        jpeg: Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]),
        width: 1,
        height: 1,
    });
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/cameras/{cam}/snapshot"),
        Some(&viewer),
        None,
    )
    .await;
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

    let bad =
        json!({"min_free_bytes": 0, "session_ttl_hours": 0, "retention_interval_seconds": 300});
    let r = call(&h.app, "PUT", "/api/v1/settings", Some(&admin), Some(bad)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let good =
        json!({"min_free_bytes": 5, "session_ttl_hours": 2, "retention_interval_seconds": 60});
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
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&manager),
        Some(camera_body("front")),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.json()["id"].as_i64().unwrap();
    assert!(r.json()["status"].is_object());

    let list = call(&h.app, "GET", "/api/v1/cameras", Some(&manager), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list
        .json()
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == id));

    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/cameras/{id}"),
        Some(&manager),
        None,
    )
    .await;
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
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/timeline?camera_id={cam}&date=2026-13-40"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/timeline?camera_id={cam}&date=2026-03-04"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json()["recordings"].is_array() && r.json()["events"].is_array());
}

#[tokio::test]
async fn web_ui_is_served() {
    let h = harness();
    let r = call(&h.app, "GET", "/", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
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
        w.write_frame(start + chrono::Duration::milliseconds(*ms), &jpeg)
            .unwrap();
    }
    let summary = w.finish().unwrap();
    let rec = h
        .state
        .store
        .create_recording(&NewRecording {
            camera_id: cam,
            path: rel.clone(),
            start,
            trigger: RecordingTrigger::Manual,
        })
        .unwrap();
    h.state
        .store
        .finish_recording(rec.id, Utc::now(), summary.frames, summary.size_bytes)
        .unwrap();

    // Playback at 16x: three MJPEG parts.
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/recordings/{}/stream?speed=16", rec.id),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .contains("boundary=ssframe"));
    let text = String::from_utf8_lossy(&r.body);
    assert_eq!(
        text.matches("--ssframe\r\nContent-Type: image/jpeg")
            .count(),
        3
    );

    // Single frame nearest at/before 150 ms is frame 1.
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/recordings/{}/frame?offset_ms=150", rec.id),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(&r.body[..], &[0xFF, 0xD8, 1, 0xFF, 0xD9]);

    // Download returns the raw .mjpg bytes as an attachment.
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/recordings/{}/download", rec.id),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .starts_with("attachment"));
    assert_eq!(
        r.body.len() as u64,
        std::fs::metadata(root.join(&rel)).unwrap().len()
    );

    // Viewers cannot delete; a manager can, and the segment files go with it.
    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/recordings/{}", rec.id),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let manager = login(&h, "manager", "password123").await;
    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/recordings/{}", rec.id),
        Some(&manager),
        None,
    )
    .await;
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
    assert!(resp.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("multipart/x-mixed-replace"));
    let mut body = resp.into_body();
    let chunk = body
        .frame()
        .await
        .expect("a chunk")
        .unwrap()
        .into_data()
        .unwrap();
    let text = &chunk[..];
    assert!(text.starts_with(b"--ssframe\r\nContent-Type: image/jpeg\r\nContent-Length: 5\r\n\r\n"));
    assert!(text.ends_with(b"\r\n"));
}

// ------------------------------------------------------------------ security regressions

#[tokio::test]
async fn viewer_cannot_delete_users() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let vid = h.state.store.get_user_by_name("viewer").unwrap().id;
    let manager = login(&h, "manager", "password123").await;
    let mid = h.state.store.get_user_by_name("manager").unwrap().id;

    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/users/{mid}"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/users/{mid}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        h.state.store.get_user(mid).is_ok(),
        "manager must still exist"
    );
    assert!(h.state.store.get_user(vid).is_ok());
}

#[tokio::test]
async fn viewer_is_forbidden_on_every_manager_and_admin_route() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let manager = login(&h, "manager", "password123").await;
    let cam = add_camera(&h, "gate");
    let rec = h
        .state
        .store
        .create_recording(&NewRecording {
            camera_id: cam,
            path: "gate/x.mjpg".into(),
            start: Utc::now(),
            trigger: RecordingTrigger::Manual,
        })
        .unwrap();
    let vid = h.state.store.get_user_by_name("viewer").unwrap().id;
    let settings =
        json!({"min_free_bytes": 5, "session_ttl_hours": 2, "retention_interval_seconds": 60});
    let new_user = json!({"username": "newbie", "password": "longenough", "role": "viewer"});

    // (method, path, body, minimum role required by SPEC §10)
    let routes: Vec<(&str, String, Option<Value>, Role)> = vec![
        (
            "POST",
            "/api/v1/cameras".into(),
            Some(camera_body("x")),
            Role::Manager,
        ),
        (
            "PUT",
            format!("/api/v1/cameras/{cam}"),
            Some(camera_body("x")),
            Role::Manager,
        ),
        (
            "DELETE",
            format!("/api/v1/cameras/{cam}"),
            None,
            Role::Manager,
        ),
        (
            "POST",
            format!("/api/v1/cameras/{cam}/enable"),
            None,
            Role::Manager,
        ),
        (
            "POST",
            format!("/api/v1/cameras/{cam}/disable"),
            None,
            Role::Manager,
        ),
        (
            "POST",
            format!("/api/v1/cameras/{cam}/record/start"),
            None,
            Role::Manager,
        ),
        (
            "POST",
            format!("/api/v1/cameras/{cam}/record/stop"),
            None,
            Role::Manager,
        ),
        (
            "GET",
            "/api/v1/discovery/onvif?timeout_ms=500".into(),
            None,
            Role::Manager,
        ),
        (
            "DELETE",
            format!("/api/v1/recordings/{}", rec.id),
            None,
            Role::Manager,
        ),
        (
            "POST",
            format!("/api/v1/recordings/{}/lock", rec.id),
            None,
            Role::Manager,
        ),
        (
            "POST",
            format!("/api/v1/recordings/{}/unlock", rec.id),
            None,
            Role::Manager,
        ),
        ("GET", "/api/v1/users".into(), None, Role::Admin),
        ("POST", "/api/v1/users".into(), Some(new_user), Role::Admin),
        (
            "PUT",
            format!("/api/v1/users/{vid}"),
            Some(json!({"role": "manager"})),
            Role::Admin,
        ),
        ("DELETE", format!("/api/v1/users/{vid}"), None, Role::Admin),
        ("GET", "/api/v1/settings".into(), None, Role::Admin),
        (
            "PUT",
            "/api/v1/settings".into(),
            Some(settings),
            Role::Admin,
        ),
    ];

    for (method, path, body, _) in &routes {
        let r = call(&h.app, method, path, Some(&viewer), body.clone()).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "viewer on {method} {path}");
        assert_eq!(r.json()["error"], "forbidden", "viewer on {method} {path}");
    }
    // Managers may use manager routes but never admin routes.
    for (method, path, body, min) in &routes {
        if *min == Role::Admin {
            let r = call(&h.app, method, path, Some(&manager), body.clone()).await;
            assert_eq!(
                r.status,
                StatusCode::FORBIDDEN,
                "manager on {method} {path}"
            );
        }
    }

    // Nothing was changed by the refused requests.
    assert!(h.state.store.get_camera(cam).is_ok());
    assert!(h.state.store.get_recording(rec.id).is_ok());
    assert_eq!(h.state.store.list_users().unwrap().len(), 3);
    assert_eq!(h.state.store.get_settings().unwrap().session_ttl_hours, 24);
}

#[tokio::test]
async fn camera_url_credentials_move_into_fields() {
    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let body = json!({"name": "cred", "source": {"type": "mjpeg", "url": "http://ad%40min:p%25ss%2Fw@127.0.0.1:9/video?x=1"}});
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&manager),
        Some(body),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.json()["id"].as_i64().unwrap();
    assert_eq!(r.json()["source"]["url"], "http://127.0.0.1:9/video?x=1");
    assert_eq!(r.json()["source"]["username"], "ad@min");
    assert_eq!(r.json()["source"]["password"], "********");
    assert!(!String::from_utf8_lossy(&r.body).contains("p%25ss"));

    match h.state.store.get_camera(id).unwrap().source {
        ss_core::CameraSource::Mjpeg {
            url,
            username,
            password,
        } => {
            assert_eq!(url, "http://127.0.0.1:9/video?x=1");
            assert_eq!(username.as_deref(), Some("ad@min"));
            assert_eq!(password.as_deref(), Some("p%ss/w"));
        }
        other => panic!("unexpected source {other:?}"),
    }
}

#[tokio::test]
async fn masked_password_only_kept_for_same_scheme_host_and_port() {
    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let body = json!({"name": "mk", "source": {"type": "mjpeg", "url": "http://127.0.0.1:9/x", "username": "u", "password": "hunter22"}});
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&manager),
        Some(body),
    )
    .await;
    let id = r.json()["id"].as_i64().unwrap();
    let stored_pw = |h: &Harness| match h.state.store.get_camera(id).unwrap().source {
        ss_core::CameraSource::Mjpeg { password, .. } => password,
        other => panic!("unexpected source {other:?}"),
    };

    // Same origin, different path: the stored password is kept.
    let same = json!({"name": "mk", "source": {"type": "mjpeg", "url": "http://127.0.0.1:9/other", "username": "u", "password": "********"}});
    let r = call(
        &h.app,
        "PUT",
        &format!("/api/v1/cameras/{id}"),
        Some(&manager),
        Some(same),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(stored_pw(&h).as_deref(), Some("hunter22"));

    // Different host, port or scheme with the mask: refused, nothing changes.
    for url in [
        "http://127.0.0.2:9/x",
        "http://127.0.0.1:10/x",
        "https://127.0.0.1:9/x",
    ] {
        let changed = json!({"name": "mk", "source": {"type": "mjpeg", "url": url, "username": "u", "password": "********"}});
        let r = call(
            &h.app,
            "PUT",
            &format!("/api/v1/cameras/{id}"),
            Some(&manager),
            Some(changed),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{url}");
        assert!(
            r.json()["message"]
                .as_str()
                .unwrap()
                .contains("re-enter the password"),
            "{url}"
        );
        assert_eq!(stored_pw(&h).as_deref(), Some("hunter22"));
    }

    // Changing the address with a new password in the URL is fine.
    let reentered = json!({"name": "mk", "source": {"type": "mjpeg", "url": "http://u:newpw@127.0.0.2:9/x", "username": "u", "password": "********"}});
    let r = call(
        &h.app,
        "PUT",
        &format!("/api/v1/cameras/{id}"),
        Some(&manager),
        Some(reentered),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(stored_pw(&h).as_deref(), Some("newpw"));
}

#[tokio::test]
async fn delete_camera_removes_row_files_and_latest_frame() {
    use ss_recorder::segment::SegmentWriter;

    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let cam = add_camera(&h, "gone");
    let root = h.state.engine.storage_root().to_path_buf();
    let start = Utc::now();
    let rel = format!("cam{cam}/gone/seg.mjpg");
    let mut w = SegmentWriter::create(&root, &rel, start).unwrap();
    w.write_frame(start, &[0xFF, 0xD8, 1, 0xFF, 0xD9]).unwrap();
    let summary = w.finish().unwrap();
    let rec = h
        .state
        .store
        .create_recording(&NewRecording {
            camera_id: cam,
            path: rel.clone(),
            start,
            trigger: RecordingTrigger::Continuous,
        })
        .unwrap();
    h.state
        .store
        .finish_recording(rec.id, Utc::now(), summary.frames, summary.size_bytes)
        .unwrap();
    h.state.engine.frames().publish(Frame {
        camera_id: cam,
        ts: Utc::now(),
        jpeg: Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]),
        width: 1,
        height: 1,
    });

    let r = call(
        &h.app,
        "DELETE",
        &format!("/api/v1/cameras/{cam}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(h.state.store.get_camera(cam).is_err());
    assert!(!root.join(&rel).exists());
    assert!(h.state.engine.frames().latest(cam).is_none());
}

#[tokio::test]
async fn snapshot_older_than_ten_seconds_is_unavailable() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "stale-cam");
    h.state.engine.frames().publish(Frame {
        camera_id: cam,
        ts: Utc::now() - chrono::TimeDelta::seconds(20),
        jpeg: Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]),
        width: 1,
        height: 1,
    });
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/cameras/{cam}/snapshot"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn password_change_revokes_other_sessions_only() {
    let h = harness();
    let viewer_a = login(&h, "viewer", "password123").await;
    let viewer_b = login(&h, "viewer", "password123").await;
    let r = call(
        &h.app,
        "PUT",
        "/api/v1/auth/password",
        Some(&viewer_a),
        Some(json!({"old_password": "password123", "new_password": "brandnew1"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some(&viewer_b), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some(&viewer_a), None).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn admin_password_reset_and_role_change_revoke_target_sessions() {
    let h = harness();
    let admin_a = login(&h, "admin", "adminpass").await;
    let admin_b = login(&h, "admin", "adminpass").await;
    let viewer = login(&h, "viewer", "password123").await;
    let vid = h.state.store.get_user_by_name("viewer").unwrap().id;
    let admin_id = h.state.store.get_user_by_name("admin").unwrap().id;

    // Password reset of another user revokes all of their sessions.
    let r = call(
        &h.app,
        "PUT",
        &format!("/api/v1/users/{vid}"),
        Some(&admin_a),
        Some(json!({"password": "resetpass1"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Role change revokes the target's sessions too.
    let viewer2 = login(&h, "viewer", "resetpass1").await;
    let r = call(
        &h.app,
        "PUT",
        &format!("/api/v1/users/{vid}"),
        Some(&admin_a),
        Some(json!({"role": "manager"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some(&viewer2), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Admin changing their own password keeps the session they used.
    let r = call(
        &h.app,
        "PUT",
        &format!("/api/v1/users/{admin_id}"),
        Some(&admin_a),
        Some(json!({"password": "adminpass2"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some(&admin_b), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = call(&h.app, "GET", "/api/v1/auth/me", Some(&admin_a), None).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn parallel_login_guesses_are_capped_at_five() {
    let h = harness();
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let app = h.app.clone();
        tasks.push(tokio::spawn(async move {
            call(
                &app,
                "POST",
                "/api/v1/auth/login",
                None,
                Some(json!({"username": "viewer", "password": "wrong-guess"})),
            )
            .await
            .status
        }));
    }
    let mut statuses = Vec::new();
    for t in tasks {
        statuses.push(t.await.unwrap());
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::UNAUTHORIZED)
            .count(),
        5
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
            .count(),
        7
    );
    // The correct password is refused while the name is blocked.
    let r = call(
        &h.app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "viewer", "password": "password123"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn unknown_usernames_are_rate_limited_like_real_ones() {
    let h = harness();
    for _ in 0..5 {
        let r = call(
            &h.app,
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"username": "ghost", "password": "whatever1"})),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = call(
        &h.app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "ghost", "password": "whatever1"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn cookie_auth_unsafe_requests_need_csrf_header() {
    let h = harness();
    let manager = login(&h, "manager", "password123").await;
    let cookie = format!("ss_session={manager}");
    let body = camera_body("csrf-cam");

    // Cookie + unsafe method without the header: 403, nothing created.
    let r = send(
        &h.app,
        "POST",
        "/api/v1/cameras",
        &[("cookie", cookie.as_str())],
        Some(body.clone()),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"], "forbidden");
    assert!(h.state.store.list_cameras().unwrap().is_empty());

    // Same request with the header goes through.
    let r = send(
        &h.app,
        "POST",
        "/api/v1/cameras",
        &[("cookie", cookie.as_str()), ("x-requested-with", "ss")],
        Some(body),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);

    // Cookie-authenticated safe methods need no header.
    let r = send(
        &h.app,
        "GET",
        "/api/v1/cameras",
        &[("cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);

    // Bearer-authenticated unsafe requests need no header (not CSRF-able).
    let r = call(
        &h.app,
        "POST",
        "/api/v1/cameras",
        Some(&manager),
        Some(camera_body("bearer-cam")),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);

    // Cookie logout needs the header too.
    let r = send(
        &h.app,
        "POST",
        "/api/v1/auth/logout",
        &[("cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &h.app,
        "POST",
        "/api/v1/auth/logout",
        &[("cookie", cookie.as_str()), ("x-requested-with", "ss")],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn query_token_only_works_on_streaming_gets() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "qt-cam");
    h.state.engine.frames().publish(Frame {
        camera_id: cam,
        ts: Utc::now(),
        jpeg: Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]),
        width: 1,
        height: 1,
    });

    // Ignored elsewhere, including on unsafe methods.
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/cameras?token={viewer}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = call(
        &h.app,
        "POST",
        &format!("/api/v1/auth/logout?token={viewer}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/cameras/{cam}?token={viewer}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Accepted on the snapshot GET (used by <img>/links).
    let r = call(
        &h.app,
        "GET",
        &format!("/api/v1/cameras/{cam}/snapshot?token={viewer}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn open_live_stream_ends_on_shutdown() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "shut-cam");
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
    let mut body = resp.into_body();
    assert!(body.frame().await.expect("first chunk").is_ok());

    h.state.request_shutdown();
    let ended = tokio::time::timeout(Duration::from_secs(5), body.collect()).await;
    assert!(ended.is_ok(), "live stream must end after shutdown");
}

#[tokio::test]
async fn sse_stream_ends_on_shutdown() {
    let h = harness();
    let viewer = login(&h, "viewer", "password123").await;
    let req = Request::builder()
        .uri("/api/v1/events/stream")
        .header(header::AUTHORIZATION, format!("Bearer {viewer}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    h.state.request_shutdown();
    let ended = tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect()).await;
    assert!(ended.is_ok(), "SSE stream must end after shutdown");
}

#[tokio::test]
async fn streams_end_when_their_session_is_revoked() {
    let h = harness_with(Some(Duration::from_millis(50)));
    let viewer = login(&h, "viewer", "password123").await;
    let cam = add_camera(&h, "revoke-cam");
    h.state.engine.frames().publish(Frame {
        camera_id: cam,
        ts: Utc::now(),
        jpeg: Bytes::from_static(&[0xFF, 0xD8, 9, 0xFF, 0xD9]),
        width: 1,
        height: 1,
    });

    // Live MJPEG and SSE, authenticated by the query token of an <img>/EventSource.
    let req = Request::builder()
        .uri(format!("/api/v1/cameras/{cam}/live?token={viewer}"))
        .body(Body::empty())
        .unwrap();
    let live = h.app.clone().oneshot(req).await.unwrap();
    let req = Request::builder()
        .uri(format!("/api/v1/events/stream?token={viewer}"))
        .body(Body::empty())
        .unwrap();
    let sse = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(live.status(), StatusCode::OK);
    assert_eq!(sse.status(), StatusCode::OK);

    h.state.store.delete_session(&viewer).unwrap();
    let live_ended = tokio::time::timeout(Duration::from_secs(5), live.into_body().collect()).await;
    assert!(
        live_ended.is_ok(),
        "live stream must end once its session is gone"
    );
    let sse_ended = tokio::time::timeout(Duration::from_secs(5), sse.into_body().collect()).await;
    assert!(
        sse_ended.is_ok(),
        "SSE stream must end once its session is gone"
    );
}

#[tokio::test]
async fn fresh_session_keeps_stream_open() {
    let h = harness_with(Some(Duration::from_millis(50)));
    let viewer = login(&h, "viewer", "password123").await;
    let req = Request::builder()
        .uri("/api/v1/events/stream")
        .header(header::AUTHORIZATION, format!("Bearer {viewer}"))
        .body(Body::empty())
        .unwrap();
    let sse = h.app.clone().oneshot(req).await.unwrap();
    // Several recheck periods pass with a valid session: the stream is still open.
    let next = tokio::time::timeout(Duration::from_millis(300), async {
        let mut body = sse.into_body();
        body.frame().await
    })
    .await;
    assert!(
        next.is_err(),
        "stream must stay open while the session is valid"
    );
}
