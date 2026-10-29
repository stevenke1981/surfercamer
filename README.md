# surveillance-rs

A self-hosted video surveillance system in Rust, modelled on Synology
**Surveillance Station**: manage IP cameras, watch them live, record on a
weekly schedule or on motion, browse a timeline, get alerts, and control access
by role — one binary (`ssd`) with an embedded web UI.

- **Plan:** [docs/PLAN.md](docs/PLAN.md) · **Spec:** [docs/SPEC.md](docs/SPEC.md) ·
  **How it was built:** [docs/AGENT_WORKFLOW.md](docs/AGENT_WORKFLOW.md)

## Features

| Area | What you get |
|------|--------------|
| Cameras | RTSP (via `ffmpeg`), HTTP MJPEG, polled JPEG snapshot, built-in synthetic test camera; ONVIF WS-Discovery |
| Live view | 1/4/9/16 grid, MJPEG in any browser, status / REC / motion badges, snapshot, manual record |
| Recording | 7×48 half-hour weekly schedule: none / continuous / motion / both; pre- and post-record; segment rotation |
| Motion | Frame differencing with sensitivity, threshold, detection regions, cooldown |
| Playback | 24 h timeline per camera/day, recordings coloured by trigger, event ticks, seekable MJPEG playback at 0.25–16×, download |
| Retention | By age and total size per camera; locked recordings are kept |
| Events | Motion, connection lost/restored, recording start/stop, storage full; live toasts over SSE; acknowledge |
| Users | admin / manager / viewer roles, argon2id passwords, sessions, login rate limiting |

## Quick start

```sh
cd surveillance-rs
cargo build --release
SS_ADMIN_PASSWORD='change-me-please' ./target/release/ssd --data-dir ./data
# open http://localhost:9900 and sign in as admin
```

On the first run, the `admin` user is created. If `SS_ADMIN_PASSWORD` is not
set, a random password is generated and printed once in the log.

To try it without a camera, add a camera with source type **Synthetic**: it
renders a moving box, so you get live video, motion events and recordings
immediately.

RTSP cameras need `ffmpeg` on `PATH` (or set `SS_FFMPEG=/path/to/ffmpeg`).

### Options

| Flag | Env | Default |
|------|-----|---------|
| `--data-dir` | `SS_DATA_DIR` | `./data` (holds `ss.db` and `recordings/`) |
| `--listen` | `SS_LISTEN` | `0.0.0.0:9900` |
| `--admin-password` | `SS_ADMIN_PASSWORD` | random, first run only |
| | `RUST_LOG` | `info` |

## Workspace

| Crate | Role |
|-------|------|
| `ss-core` | Domain model, `Store` trait, `FrameHub`, schedule, credential redaction |
| `ss-db` | SQLite `Store` (rusqlite, WAL, migrations) |
| `ss-camera` | Frame sources, JPEG splitter, ONVIF discovery |
| `ss-motion` | Motion detection |
| `ss-recorder` | `.mjpg` + `.idx` segments, record policy, pre-buffer, retention, disk space |
| `ss-engine` | Per-camera workers, events, retention loop, crash recovery |
| `ss-server` | axum REST API, MJPEG/SSE streaming, auth, embedded UI, `ssd` binary |

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Security notes

- Serve behind HTTPS (reverse proxy) when exposed beyond a trusted LAN; the
  session cookie is not marked `Secure`.
- RTSP credentials are passed to `ffmpeg` in the URL and are visible to local
  users via the process list.
- Camera passwords are stored in the SQLite database in clear text (they must
  be replayed to the camera); protect the data directory.

## Not yet implemented

H.264 passthrough / MP4 / HLS, PTZ, audio, push/e-mail notifications, object
detection, multi-server CMS. See the milestones in [docs/PLAN.md](docs/PLAN.md).
