# surveillance-rs — Technical Specification (v0.1)

Normative words: MUST / SHOULD / MAY. Rust signatures are fixed by the stubs
in `crates/*/src`; this document defines behaviour. `ss-core` types are the
source of truth for all JSON shapes.

## 1. Runtime & layout

* Binary `ssd` (crate `ss-server`). CLI (clap, each also an env var):
  * `--data-dir <PATH>` / `SS_DATA_DIR`, default `./data`
  * `--listen <ADDR>` / `SS_LISTEN`, default `0.0.0.0:9900`
  * `--admin-password <PW>` / `SS_ADMIN_PASSWORD` — used only when the user
    table is empty; if absent a random 16-char password is generated and
    printed once to the log.
* Data dir: `ss.db` (SQLite), `recordings/` (segment root).
* Logging: `tracing` + `tracing-subscriber` (`RUST_LOG`, default `info`).
* Graceful shutdown on SIGINT/SIGTERM: stop HTTP, `Engine::shutdown()` (finishes open segments).

## 2. Domain model (ss-core)

See `crates/ss-core/src/model.rs`. Key points:

* IDs are `i64` SQLite rowids. Times are `DateTime<Utc>`, RFC 3339 in JSON.
* `CameraSource` is internally tagged: `{"type":"rtsp","url":…}` etc.
  API responses MUST use `source.redacted()`.
* `Schedule` serialises as an array of exactly 336 modes
  (`"none"|"continuous"|"motion"|"continuous_and_motion"`), index =
  `weekday*48 + slot`, Monday = 0, evaluated in server local time.
* `CameraInput::validate()` MUST be called before create/update.
* `User.password_hash` is never serialised.

## 3. Frame pipeline

`FrameSource` → `FrameHub::publish` (per-camera broadcast, capacity 32,
latest frame retained) → subscribers: motion task, recorder task, live-view
HTTP streams. Subscribers that lag (`RecvError::Lagged`) MUST continue, not exit.

## 4. Errors → HTTP

| `ss_core::Error` | HTTP | body |
|---|---|---|
| NotFound | 404 | `{"error":"not_found","message":…}` |
| Invalid | 400 | `invalid` |
| Conflict | 409 | `conflict` |
| Unauthorized | 401 | `unauthorized` |
| Forbidden | 403 | `forbidden` |
| Camera | 502 | `camera` |
| Storage, Database, Io, Internal | 500 | `internal` (message logged, generic text returned) |

## 5. Persistence (ss-db)

SQLite, `PRAGMA journal_mode=WAL; foreign_keys=ON; busy_timeout=5000`.
Migrations tracked with `PRAGMA user_version` (v1 schema below).

```sql
CREATE TABLE cameras (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, enabled INTEGER NOT NULL,
  source TEXT NOT NULL,        -- JSON CameraSource
  recording TEXT NOT NULL,     -- JSON RecordingConfig
  motion TEXT NOT NULL,        -- JSON MotionConfig
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE recordings (
  id INTEGER PRIMARY KEY, camera_id INTEGER NOT NULL REFERENCES cameras(id) ON DELETE CASCADE,
  path TEXT NOT NULL UNIQUE, start TEXT NOT NULL, "end" TEXT, frames INTEGER NOT NULL DEFAULT 0,
  size_bytes INTEGER NOT NULL DEFAULT 0, trigger TEXT NOT NULL, locked INTEGER NOT NULL DEFAULT 0);
CREATE INDEX recordings_cam_start ON recordings(camera_id, start);
CREATE TABLE events (
  id INTEGER PRIMARY KEY, camera_id INTEGER REFERENCES cameras(id) ON DELETE CASCADE,
  kind TEXT NOT NULL, at TEXT NOT NULL, score REAL NOT NULL, message TEXT NOT NULL,
  recording_id INTEGER REFERENCES recordings(id) ON DELETE SET NULL,
  acknowledged INTEGER NOT NULL DEFAULT 0);
CREATE INDEX events_at ON events(at);
CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE,
  password_hash TEXT NOT NULL, role TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE sessions (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL, expires_at TEXT NOT NULL);
CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id = 1), value TEXT NOT NULL);
```

* Timestamps stored as RFC 3339 with fixed `%Y-%m-%dT%H:%M:%S%.6fZ` so that
  string comparison orders correctly.
* Unique violations → `Conflict`; missing rows → `NotFound`.
* `list_recordings`: overlap filter = `start < to AND (end IS NULL OR end >= from)`;
  default limit 100, max 1000. `list_events` similar with `at` in `[from,to)`.
* `get_settings` returns `Settings::default()` if no row.
* `get_session` returns NotFound if `expires_at <= now`.

## 6. Camera sources (ss-camera)

* `connect()` dispatches on `CameraSource`: Rtsp→`RtspSource::spawn`
  (fps = 10), Mjpeg→`MjpegHttpSource::connect`, Snapshot→`SnapshotSource::new`,
  Synthetic→`SyntheticSource::new`. Connection errors → `Error::Camera`.
* **JpegSplitter**: scan for `FF D8` start; for each start, find the next
  `FF D9` end and emit `[start..=end]`. Bytes before a start are discarded.
  Buffer is bounded by `MAX_JPEG_BYTES` (drop everything if exceeded with no
  end). Must handle markers split across `push` calls.
* **jpeg::dimensions**: walk markers from `FFD8`; return height/width from
  the first SOF0..SOF15 (excluding C4, C8, CC).
* **MjpegHttpSource**: reqwest GET with optional basic auth; non-2xx →
  `Camera` error; read `bytes_stream()` chunks into a `JpegSplitter`; stream
  end → `Camera("stream ended")`. 10 s connect timeout; 15 s without a frame → error.
* **SnapshotSource**: GET every `interval` (tokio interval, missed ticks
  delayed); body must be a JPEG (`dimensions` succeeds) else `Camera` error.
* **RtspSource**: `tokio::process::Command` on `$SS_FFMPEG` or `ffmpeg` with
  `ffmpeg_args`: `-hide_banner -loglevel error -rtsp_transport tcp -i <url>
  -an -f image2pipe -c:v mjpeg -q:v 5 -r <fps> pipe:1`; `kill_on_drop(true)`;
  stdout through `JpegSplitter`; EOF → `Camera` error with last stderr line.
  Credentials percent-encoded into `rtsp://user:pass@host…` (replace any
  existing userinfo).
* **SyntheticSource**: renders `width×height` RGB: horizontal gradient
  background, a 1/6-size white box moving horizontally (period 5 s at the
  configured fps), encoded with `image`'s JpegEncoder (quality 70). Paced with
  `tokio::time::interval(1/fps)`. Every 10 s of frames the box pauses for
  3 s (gives "no motion" periods for tests).
* **ONVIF discovery**: bind UDP `0.0.0.0:0`, send `probe_message(uuid)` to
  `239.255.255.250:3702`, collect `ProbeMatches` until timeout; dedupe by
  endpoint. Parse with simple tag scanning (no XML crate): elements whose
  local name is `EndpointReference/Address`, `XAddrs`, `Scopes` (whitespace-
  separated lists); `name` = percent-decoded value after `onvif://www.onvif.org/name/`.
* Every `Frame` carries `ts = Utc::now()` at receipt and dimensions from
  `jpeg::dimensions` (0×0 if unknown).

## 7. Motion detection (ss-motion)

1. Decode JPEG (`image`), convert to Luma8, resize to width `ANALYSIS_WIDTH`
   (height by aspect, min 1) with `FilterType::Triangle`.
2. Background model: per-pixel running average `bg = bg*0.9 + cur*0.1`
   (f32). First frame (or size change, or after `reset`) initialises bg and
   returns `{score:0, triggered:false}`.
3. Changed pixel: `|cur - bg| > sensitivity_to_delta(sensitivity)` where
   `delta = round(100 - (s-1) * 95/98)` (s=1→100, s=99→5).
4. Analysed area = union of `regions` (normalised → pixel rects, clamped);
   whole frame if empty. `score = changed / analysed_pixels`.
5. `triggered = enabled && score*100 >= threshold`. Disabled config returns
   score as computed but `triggered=false`.
6. Cooldown is NOT handled here (engine applies `cooldown_seconds`).

## 8. Recording (ss-recorder)

* **Segment files**: see module docs in `segment.rs`. Path
  `cam{id}/{YYYY-MM-DD}/{HHMMSS}-{mmm}.mjpg` (UTC). Writer uses `BufWriter`;
  each frame appends data then index record. `size_bytes` = data + index bytes.
  Reader validates entries (`byte_offset+len <= data_len`) and drops partial
  trailing records.
* **RecordPolicy**: stores `last_motion: Option<DateTime>` and `manual: bool`.
  `motion_active(now, post)` = `last_motion` exists and `now - last_motion <= post` s.
  `decide` priority Manual > Motion > Continuous > None.
* **PreBuffer**: VecDeque; on push evict frames older than `newest - seconds`.
  `seconds == 0` keeps nothing.
* **Retention** `plan`: (a) age: `end < now - max_days days`; (b) size: oldest
  first while `usage - freed > max_bytes`. A recording selected by (a) counts
  towards `freed`. Returns ids in oldest-first order without duplicates.
* `delete_segment_files` removes `.mjpg`, `.idx`, and the parent dir if empty.
* `disk_space` uses `libc::statvfs` (`f_bavail * f_frsize`, `f_blocks * f_frsize`).

## 9. Engine (ss-engine)

Per enabled camera, one supervisor task:

1. Status `Connecting`; `ss_camera::connect`. On error: status `Offline`,
   `last_error`, emit `ConnectionLost` (only on Online→Offline transition, or
   the first failure), retry with backoff 1 s, 2 s, 4 s … max 30 s.
2. On first frame after (re)connect: status `Online`; emit
   `ConnectionRestored` if it was previously Offline.
3. Each frame: publish to `FrameHub`; update fps (EWMA) and `last_frame_at`.
4. Motion task (subscribed to hub) if `motion.enabled`: run detector on at most
   5 fps (skip frames). On `triggered`: `policy.on_motion(now)`; emit a
   `Motion` event (score, message "Motion detected") unless one was emitted
   less than `cooldown_seconds` ago; `motion_active` status flag follows
   `policy.motion_active`.
5. Recorder task (subscribed to hub): throttle to `max_fps`; push every frame
   into PreBuffer(`pre_record_seconds`); `decide(now, schedule.mode_at(now),
   post_record_seconds)`:
   * becomes Some(trigger) and no open segment → open segment (store
     `create_recording`), emit `RecordingStarted`; for Motion, first write the
     drained pre-buffer.
   * trigger changes, or segment older than `segment_seconds` → finish
     (`finish_recording`) and open a new one.
   * becomes None → finish, emit `RecordingStopped`.
   * Before opening, if `disk_space(root).free < settings.min_free_bytes`:
     don't record, emit `StorageFull` at most once per 10 min.
   * The policy decision is also re-evaluated every 1 s even without frames
     (so a stream stall closes motion segments).
6. Motion events created while a segment is open get `recording_id` set.
7. `shutdown`/`reload_camera` cancel tasks (tokio `CancellationToken`-style
   watch channel or `JoinHandle::abort` after finishing segments) and finish
   open segments cleanly.
8. `start()`: any recording with `end = NULL` (crash) is finished using its
   segment's actual index (frames/size/end from `SegmentReader`), or deleted
   if the file is missing/empty.
9. Retention loop every `settings.retention_interval_seconds`: per camera
   `plan(retention_candidates, policy, recording_usage)`; delete files then
   rows. Also delete events older than 90 days.
10. Events: `emit_event` persists then sends on a `broadcast::channel(256)`.

## 10. HTTP API (ss-server)

Base `/api/v1`, JSON. Auth: `Authorization: Bearer <token>` or cookie
`ss_session=<token>` or query `?token=` (for `<img>`/EventSource). Tokens: 32
random bytes hex. Passwords: argon2id PHC. Roles: V=viewer, M=manager, A=admin
(higher includes lower).

| Method & path | Role | Description |
|---|---|---|
| POST `/auth/login` `{username,password}` | – | → `{token, user, expires_at}`; sets `ss_session` cookie (HttpOnly, SameSite=Lax) |
| POST `/auth/logout` | V | deletes session, clears cookie |
| GET `/auth/me` | V | current user |
| GET `/cameras` | V | `[{…Camera(redacted), status}]` |
| POST `/cameras` `CameraInput` | M | 201 + camera; engine `reload_camera` |
| GET `/cameras/{id}` | V | camera + status |
| PUT `/cameras/{id}` `CameraInput` | M | replace; a password of `"********"` keeps the stored password; reload |
| DELETE `/cameras/{id}` | M | stop worker, delete segment files + rows, 204 |
| POST `/cameras/{id}/enable` · `/disable` | M | toggle + reload |
| GET `/cameras/{id}/snapshot` | V | latest frame `image/jpeg`; 503 if none |
| GET `/cameras/{id}/live` | V | `multipart/x-mixed-replace; boundary=ssframe` stream from FrameHub |
| POST `/cameras/{id}/record/start` · `/stop` | M | manual recording |
| GET `/discovery/onvif?timeout_ms=3000` | M | `[DiscoveredDevice]` (timeout clamp 500..10000) |
| GET `/recordings?camera_id&from&to&trigger&locked&limit&offset` | V | list |
| GET `/recordings/{id}` | V | one |
| DELETE `/recordings/{id}` | M | 409 if locked or still open; deletes files |
| POST `/recordings/{id}/lock` · `/unlock` | M | |
| GET `/recordings/{id}/stream?offset_ms&speed` | V | MJPEG playback paced by index timestamps / speed (0.25..16) |
| GET `/recordings/{id}/frame?offset_ms` | V | single JPEG nearest at/before offset |
| GET `/recordings/{id}/download` | V | raw `.mjpg`, `Content-Disposition: attachment` |
| GET `/timeline?camera_id&date=YYYY-MM-DD` | V | `{recordings:[…], events:[…]}` for that local day |
| GET `/events?camera_id&kind&from&to&acknowledged&limit&offset` | V | list |
| POST `/events/{id}/ack` | V | acknowledge |
| GET `/events/stream` | V | SSE, `event: <kind>` `data: <Event JSON>`; keep-alive 15 s |
| GET `/users` · POST `/users` `{username,password,role}` | A | password ≥ 8 chars |
| PUT `/users/{id}` `{password?,role?}` | A | cannot demote/delete the last admin (409) |
| DELETE `/users/{id}` | A | also not yourself (409) |
| PUT `/auth/password` `{old_password,new_password}` | V | change own password |
| GET `/settings` · PUT `/settings` | A | `Settings` |
| GET `/system/info` | V | `{version, uptime_seconds, cameras:{total,online,recording}, storage:{root, used_bytes, total_bytes, free_bytes}, ffmpeg: bool}` |
| GET `/health` (no prefix, no auth) | – | `ok` |

Login rate limiting: after 5 failures for a username within 5 min, reject
with 429 for 5 min (in-memory).

## 11. Web UI

Single page at `/` (HTML/CSS/JS embedded with `include_str!` from
`crates/ss-server/web/`), no build step, no external CDNs. Dark theme similar
to SS. Views (hash routing):

* **Login** — form; token kept in cookie (server sets it).
* **Live View** (`#/live`) — layout picker 1/4/9/16; each tile `<img
  src="/api/v1/cameras/{id}/live">` with name, status dot, REC badge, motion
  flash; click → single view with snapshot & manual record buttons.
* **Timeline / Playback** (`#/playback`) — camera + date picker; 24 h bar
  with recording blocks (colour by trigger) and event ticks; click → plays
  `/recordings/{id}/stream?offset_ms=` in an `<img>`; speed selector; lock /
  download / delete.
* **Cameras** (`#/cameras`, M) — table + add/edit form (source type
  switch, motion settings, regions as numbers, retention, schedule grid 7×48
  clickable with mode brush), ONVIF search button.
* **Events** (`#/events`) — filterable list, ack; live toasts via SSE on every page.
* **Users** (`#/users`, A), **Settings** (`#/settings`, A), **System** info panel.

## 12. Testing

* Unit tests in every crate (`cargo test -p <crate>`); no network required —
  HTTP sources tested against a local `tokio::net::TcpListener` mock server.
* ss-server integration tests with `tower::ServiceExt::oneshot` on `router()`
  using `SqliteStore::open_in_memory()` and a temp storage dir.
* End-to-end (engine): synthetic camera at small size records ≥1 segment and
  produces motion events within a few seconds.
