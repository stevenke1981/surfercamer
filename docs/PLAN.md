# surveillance-rs — Project Plan

A self-hosted video surveillance system written in Rust, modelled on Synology
**Surveillance Station** (SS): manage IP cameras, watch them live, record on a
weekly schedule or on motion, browse a timeline, get alerts, and control
access by user role — as one static binary with an embedded web UI.

## 1. Goals

| # | Goal | SS feature it mirrors |
|---|------|-----------------------|
| G1 | Add/edit/remove cameras (RTSP, MJPEG, JPEG snapshot, synthetic test camera) | IP Camera |
| G2 | ONVIF WS-Discovery of cameras on the LAN | "Search" in Add Camera wizard |
| G3 | Live view grid (1/4/9/16) in the browser | Live View |
| G4 | Weekly recording schedule (7×48 half-hour slots): none / continuous / motion / both | Recording Schedule |
| G5 | Motion detection with sensitivity, threshold, detection regions, cooldown | Motion Detection (by Surveillance Station) |
| G6 | Pre/post-record around motion; manual record button | Pre/Post-recording, Manual Recording |
| G7 | Segmented recordings with index → seekable playback & download | Recording / Timeline |
| G8 | Retention by age and by size; lock recordings to protect them | Rotation settings, Lock |
| G9 | Event log + live alerts (SSE) | Alert / Notification Center |
| G10 | Users with roles admin / manager / viewer; session auth | Privilege profiles |
| G11 | System info: storage usage, disk free, camera states | Home / Log center |

### Non-goals (v0.1)
PTZ control, two-way audio, audio recording, H.264 passthrough storage,
deep-learning detection (people/vehicle), mobile app, CMS/failover, VisualStation,
license limits. All are listed as later milestones (§5).

## 2. Architecture

```
            ┌─────────────── ss-server (axum, bin `ssd`) ───────────────┐
 browser ──▶│  /api/v1 REST · MJPEG live/playback · SSE · embedded UI    │
            └───────────────┬──────────────────────────┬─────────────────┘
                            │                          │
                     ┌──────▼──────┐            ┌──────▼──────┐
                     │  ss-engine  │───────────▶│   ss-db     │ SQLite (Store)
                     │ per-camera  │            └─────────────┘
                     │  workers    │
                     └─┬────┬────┬─┘
            ┌──────────┘    │    └──────────┐
     ┌──────▼─────┐  ┌──────▼─────┐  ┌──────▼──────┐
     │ ss-camera  │  │ ss-motion  │  │ ss-recorder │ .mjpg + .idx segments
     │ sources    │  │ frame diff │  │ policy/ret. │
     └────────────┘  └────────────┘  └─────────────┘
                 all depend on ss-core (model, Store trait, FrameHub)
```

Design decisions (ADR-lite):

1. **JPEG as the internal frame format.** Every source is normalised to JPEG
   (RTSP via `ffmpeg -f image2pipe -c:v mjpeg`). One format serves live view
   (MJPEG over HTTP works in every `<img>` tag), motion detection (decode +
   downscale) and storage. Trade-off: larger files than H.264; acceptable for
   v0.1 and avoids a codec dependency in-process. H.264 passthrough is M5.
2. **Own segment container** (`.mjpg` + 16-byte-record `.idx`). Append-only,
   crash-tolerant, O(1) seek by time, trivially streamable. Export to MP4 is
   delegated to ffmpeg when present.
3. **SQLite via rusqlite (bundled)** behind a sync `Store` trait — local, no
   server, trivially testable with `:memory:`.
4. **tokio broadcast `FrameHub`** per camera — slow consumers lag and skip
   frames instead of back-pressuring the camera.
5. **Embedded single-page UI** (vanilla JS, no build step) served from the
   binary via `include_str!`.

## 3. Milestones

| Milestone | Content | Status |
|-----------|---------|--------|
| M0 Foundation | Workspace, ss-core model + contracts, plan/spec | this PR |
| M1 Components | ss-db, ss-camera, ss-motion, ss-recorder (parallel) | this PR |
| M2 Runtime | ss-engine workers, retention loop, events | this PR |
| M3 Product | ss-server REST/SSE/MJPEG, auth, web UI, `ssd` binary | this PR |
| M4 Hardening | review fixes, integration tests, docs | this PR |
| M5 Later | H.264 passthrough + MP4/HLS, PTZ (ONVIF PTZ), notifications (email/webhook/Telegram), object detection, audio, multi-node CMS | backlog |

## 4. Team & workflow

See [AGENT_WORKFLOW.md](AGENT_WORKFLOW.md). Roles: **Lead/Architect** (Opus)
owns plan, spec, ss-core contracts, integration and final verification;
**Implementers** (Haiku) each own one crate; **Reviewer** (Sonnet) audits the
whole workspace; fixes loop back to implementers.

## 5. Acceptance criteria (v0.1)

- `cargo build --release` produces `ssd`; `cargo test --workspace` passes;
  `cargo clippy --workspace` has no errors.
- `ssd --data-dir ./data` starts, creates `admin` on first run, serves the UI on
  `:9900`.
- Adding a synthetic camera shows a moving live image, produces continuous
  segments under `data/recordings`, and motion events appear in the event list.
- Recordings can be listed, played back (MJPEG), downloaded, locked, deleted;
  retention removes old unlocked recordings.
- Viewer users cannot modify cameras/users; unauthenticated requests get 401.

## 6. Risks

| Risk | Mitigation |
|------|------------|
| RTSP needs ffmpeg installed | Detected at runtime; clear error in camera status; synthetic/MJPEG work without it |
| MJPEG storage is large | `max_fps` per camera, retention by size; H.264 in M5 |
| Parallel agents diverge on interfaces | All cross-crate types/traits fixed in ss-core + compiled stubs before fan-out |
| Clock jumps break timeline | Segment offsets are relative ms; DB stores UTC |
