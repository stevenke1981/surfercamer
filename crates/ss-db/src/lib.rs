//! SQLite implementation of [`ss_core::Store`]. See docs/SPEC.md §5.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use rusqlite::types::{Type, Value};
use rusqlite::{params, params_from_iter, Connection, ErrorCode, OptionalExtension, Row};
use serde::de::DeserializeOwned;
use ss_core::*;

#[cfg(test)]
mod tests;

/// Fixed-width RFC 3339 format so that string comparison orders correctly.
const TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";
const SCHEMA_VERSION: i64 = 1;
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;

const SCHEMA_V1: &str = r#"
CREATE TABLE cameras (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, enabled INTEGER NOT NULL,
  source TEXT NOT NULL,
  recording TEXT NOT NULL,
  motion TEXT NOT NULL,
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
"#;

const SELECT_CAMERA: &str =
    "SELECT id, name, enabled, source, recording, motion, created_at, updated_at FROM cameras";
const SELECT_RECORDING: &str =
    "SELECT id, camera_id, path, start, \"end\", frames, size_bytes, trigger, locked FROM recordings";
const SELECT_EVENT: &str =
    "SELECT id, camera_id, kind, at, score, message, recording_id, acknowledged FROM events";
const SELECT_USER: &str = "SELECT id, username, password_hash, role, created_at FROM users";

/// Thread-safe SQLite store. One connection behind a mutex; WAL mode.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) the database file and run migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut conn = Connection::open(path).map_err(db_err)?;
        init(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory().map_err(db_err)?;
        init(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| Error::Internal("database lock poisoned".into()))
    }
}

impl Store for SqliteStore {
    // ------------------------------------------------------------ cameras

    fn create_camera(&self, input: &CameraInput) -> Result<Camera> {
        let conn = self.lock()?;
        let now = format_ts(&Utc::now());
        let source = to_json(&input.source)?;
        let recording = to_json(&input.recording)?;
        let motion = to_json(&input.motion)?;
        conn.execute(
            "INSERT INTO cameras (name, enabled, source, recording, motion, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                input.name,
                input.enabled,
                source,
                recording,
                motion,
                now,
                now
            ],
        )
        .map_err(db_err)?;
        camera_by_id(&conn, conn.last_insert_rowid())
    }

    fn get_camera(&self, id: CameraId) -> Result<Camera> {
        let conn = self.lock()?;
        camera_by_id(&conn, id)
    }

    fn list_cameras(&self) -> Result<Vec<Camera>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!("{SELECT_CAMERA} ORDER BY id"))
            .map_err(db_err)?;
        let rows = stmt.query_map([], camera_from_row).map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    fn update_camera(&self, id: CameraId, input: &CameraInput) -> Result<Camera> {
        let conn = self.lock()?;
        let now = format_ts(&Utc::now());
        let source = to_json(&input.source)?;
        let recording = to_json(&input.recording)?;
        let motion = to_json(&input.motion)?;
        let n = conn
            .execute(
                "UPDATE cameras SET name = ?1, enabled = ?2, source = ?3, recording = ?4,
                 motion = ?5, updated_at = ?6 WHERE id = ?7",
                params![
                    input.name,
                    input.enabled,
                    source,
                    recording,
                    motion,
                    now,
                    id
                ],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("camera {id}")));
        }
        camera_by_id(&conn, id)
    }

    fn delete_camera(&self, id: CameraId) -> Result<()> {
        let conn = self.lock()?;
        let n = conn
            .execute("DELETE FROM cameras WHERE id = ?1", params![id])
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("camera {id}")));
        }
        Ok(())
    }

    // --------------------------------------------------------- recordings

    fn create_recording(&self, rec: &NewRecording) -> Result<Recording> {
        let conn = self.lock()?;
        let start = format_ts(&rec.start);
        conn.execute(
            "INSERT INTO recordings (camera_id, path, start, trigger) VALUES (?1, ?2, ?3, ?4)",
            params![rec.camera_id, rec.path, start, rec.trigger.as_str()],
        )
        .map_err(db_err)?;
        recording_by_id(&conn, conn.last_insert_rowid())
    }

    fn finish_recording(
        &self,
        id: RecordingId,
        end: DateTime<Utc>,
        frames: u64,
        size_bytes: u64,
    ) -> Result<Recording> {
        let conn = self.lock()?;
        let frames = to_i64(frames, "frames")?;
        let size_bytes = to_i64(size_bytes, "size_bytes")?;
        let end = format_ts(&end);
        let n = conn
            .execute(
                "UPDATE recordings SET \"end\" = ?1, frames = ?2, size_bytes = ?3 WHERE id = ?4",
                params![end, frames, size_bytes, id],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("recording {id}")));
        }
        recording_by_id(&conn, id)
    }

    fn get_recording(&self, id: RecordingId) -> Result<Recording> {
        let conn = self.lock()?;
        recording_by_id(&conn, id)
    }

    fn list_recordings(&self, q: &RecordingQuery) -> Result<Vec<Recording>> {
        let conn = self.lock()?;
        let mut conds: Vec<&str> = Vec::new();
        let mut args: Vec<Value> = Vec::new();

        if let Some(camera_id) = q.camera_id {
            conds.push("camera_id = ?");
            args.push(Value::Integer(camera_id));
        }
        if let Some(to) = q.to {
            // Overlap: the recording starts before the window ends.
            conds.push("start < ?");
            args.push(Value::Text(format_ts(&to)));
        }
        if let Some(from) = q.from {
            // ... and has not ended before the window starts (open recordings never end).
            conds.push("(\"end\" IS NULL OR \"end\" >= ?)");
            args.push(Value::Text(format_ts(&from)));
        }
        if let Some(trigger) = q.trigger {
            conds.push("trigger = ?");
            args.push(Value::Text(trigger.as_str().to_owned()));
        }
        if let Some(locked) = q.locked {
            conds.push("locked = ?");
            args.push(Value::Integer(i64::from(locked)));
        }

        let mut sql = String::from(SELECT_RECORDING);
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(" ORDER BY start DESC, id DESC LIMIT ? OFFSET ?");
        args.push(Value::Integer(i64::from(clamp_limit(q.limit))));
        args.push(Value::Integer(i64::from(q.offset.unwrap_or(0))));

        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), recording_from_row)
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    fn set_recording_locked(&self, id: RecordingId, locked: bool) -> Result<Recording> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE recordings SET locked = ?1 WHERE id = ?2",
                params![locked, id],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("recording {id}")));
        }
        recording_by_id(&conn, id)
    }

    fn delete_recording(&self, id: RecordingId) -> Result<()> {
        let conn = self.lock()?;
        let n = conn
            .execute("DELETE FROM recordings WHERE id = ?1", params![id])
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("recording {id}")));
        }
        Ok(())
    }

    fn recording_usage(&self, camera_id: Option<CameraId>) -> Result<u64> {
        let conn = self.lock()?;
        let total: i64 = match camera_id {
            Some(camera_id) => conn.query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM recordings WHERE camera_id = ?1",
                params![camera_id],
                |r| r.get(0),
            ),
            None => conn.query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM recordings",
                [],
                |r| r.get(0),
            ),
        }
        .map_err(db_err)?;
        u64::try_from(total).map_err(|_| Error::Database(format!("negative usage {total}")))
    }

    fn retention_candidates(&self, camera_id: CameraId) -> Result<Vec<Recording>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "{SELECT_RECORDING} WHERE camera_id = ?1 AND locked = 0 AND \"end\" IS NOT NULL
                 ORDER BY start ASC, id ASC"
            ))
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![camera_id], recording_from_row)
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    // ------------------------------------------------------------- events

    fn create_event(&self, ev: &NewEvent) -> Result<Event> {
        if !ev.score.is_finite() {
            return Err(Error::Invalid("event score must be finite".into()));
        }
        let conn = self.lock()?;
        let at = format_ts(&ev.at);
        conn.execute(
            "INSERT INTO events (camera_id, kind, at, score, message, recording_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                ev.camera_id,
                ev.kind.as_str(),
                at,
                f64::from(ev.score),
                ev.message,
                ev.recording_id
            ],
        )
        .map_err(db_err)?;
        event_by_id(&conn, conn.last_insert_rowid())
    }

    fn list_events(&self, q: &EventQuery) -> Result<Vec<Event>> {
        let conn = self.lock()?;
        let mut conds: Vec<&str> = Vec::new();
        let mut args: Vec<Value> = Vec::new();

        if let Some(camera_id) = q.camera_id {
            conds.push("camera_id = ?");
            args.push(Value::Integer(camera_id));
        }
        if let Some(kind) = q.kind {
            conds.push("kind = ?");
            args.push(Value::Text(kind.as_str().to_owned()));
        }
        if let Some(from) = q.from {
            conds.push("at >= ?");
            args.push(Value::Text(format_ts(&from)));
        }
        if let Some(to) = q.to {
            conds.push("at < ?");
            args.push(Value::Text(format_ts(&to)));
        }
        if let Some(acknowledged) = q.acknowledged {
            conds.push("acknowledged = ?");
            args.push(Value::Integer(i64::from(acknowledged)));
        }

        let mut sql = String::from(SELECT_EVENT);
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(" ORDER BY at DESC, id DESC LIMIT ? OFFSET ?");
        args.push(Value::Integer(i64::from(clamp_limit(q.limit))));
        args.push(Value::Integer(i64::from(q.offset.unwrap_or(0))));

        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), event_from_row)
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    fn acknowledge_event(&self, id: EventId) -> Result<Event> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE events SET acknowledged = 1 WHERE id = ?1",
                params![id],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("event {id}")));
        }
        event_by_id(&conn, id)
    }

    fn set_event_recording(&self, id: EventId, recording_id: RecordingId) -> Result<()> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE events SET recording_id = ?2 WHERE id = ?1",
                params![id, recording_id],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("event {id}")));
        }
        Ok(())
    }

    fn delete_events_before(&self, before: DateTime<Utc>) -> Result<u64> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "DELETE FROM events WHERE at < ?1",
                params![format_ts(&before)],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }

    // -------------------------------------------------------- users & sessions

    fn create_user(&self, username: &str, password_hash: &str, role: Role) -> Result<User> {
        let conn = self.lock()?;
        let now = format_ts(&Utc::now());
        conn.execute(
            "INSERT INTO users (username, password_hash, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![username, password_hash, role.as_str(), now],
        )
        .map_err(db_err)?;
        user_by_id(&conn, conn.last_insert_rowid())
    }

    fn get_user(&self, id: UserId) -> Result<User> {
        let conn = self.lock()?;
        user_by_id(&conn, id)
    }

    fn get_user_by_name(&self, username: &str) -> Result<User> {
        let conn = self.lock()?;
        // `username` is declared COLLATE NOCASE, so this comparison is case-insensitive.
        conn.query_row(
            &format!("{SELECT_USER} WHERE username = ?1"),
            params![username],
            user_from_row,
        )
        .optional()
        .map_err(db_err)?
        .ok_or_else(|| Error::NotFound(format!("user {username}")))
    }

    fn list_users(&self) -> Result<Vec<User>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!("{SELECT_USER} ORDER BY id"))
            .map_err(db_err)?;
        let rows = stmt.query_map([], user_from_row).map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    fn update_user(
        &self,
        id: UserId,
        password_hash: Option<&str>,
        role: Option<Role>,
    ) -> Result<User> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE users SET password_hash = COALESCE(?1, password_hash),
                 role = COALESCE(?2, role) WHERE id = ?3",
                params![password_hash, role.map(|r| r.as_str()), id],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("user {id}")));
        }
        user_by_id(&conn, id)
    }

    fn delete_user(&self, id: UserId) -> Result<()> {
        let conn = self.lock()?;
        let n = conn
            .execute("DELETE FROM users WHERE id = ?1", params![id])
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("user {id}")));
        }
        Ok(())
    }

    fn count_users(&self) -> Result<u64> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .map_err(db_err)?;
        u64::try_from(n).map_err(|_| Error::Database(format!("negative count {n}")))
    }

    fn create_session(&self, session: &Session) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                session.token,
                session.user_id,
                format_ts(&session.created_at),
                format_ts(&session.expires_at)
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    fn get_session(&self, token: &str) -> Result<Session> {
        let conn = self.lock()?;
        let now = format_ts(&Utc::now());
        conn.query_row(
            "SELECT token, user_id, created_at, expires_at FROM sessions
             WHERE token = ?1 AND expires_at > ?2",
            params![token, now],
            session_from_row,
        )
        .optional()
        .map_err(db_err)?
        .ok_or_else(|| Error::NotFound("session".into()))
    }

    fn delete_session(&self, token: &str) -> Result<()> {
        let conn = self.lock()?;
        let n = conn
            .execute("DELETE FROM sessions WHERE token = ?1", params![token])
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound("session".into()));
        }
        Ok(())
    }

    fn delete_sessions_for_user(&self, user_id: UserId, except_token: Option<&str>) -> Result<u64> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "DELETE FROM sessions WHERE user_id = ?1 AND token IS NOT ?2",
                params![user_id, except_token],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }

    fn purge_expired_sessions(&self, now: DateTime<Utc>) -> Result<u64> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "DELETE FROM sessions WHERE expires_at <= ?1",
                params![format_ts(&now)],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }

    // ----------------------------------------------------------- settings

    fn get_settings(&self) -> Result<Settings> {
        let conn = self.lock()?;
        let json: Option<String> = conn
            .query_row("SELECT value FROM settings WHERE id = 1", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        match json {
            None => Ok(Settings::default()),
            Some(json) => serde_json::from_str(&json)
                .map_err(|e| Error::Storage(format!("corrupt settings row: {e}"))),
        }
    }

    fn put_settings(&self, settings: &Settings) -> Result<()> {
        let conn = self.lock()?;
        let json = to_json(settings)?;
        conn.execute(
            "INSERT INTO settings (id, value) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET value = excluded.value",
            params![json],
        )
        .map_err(db_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

/// Pragmas and schema migrations. Runs once per connection.
fn init(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )
    .map_err(db_err)?;
    migrate(conn)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(db_err)?;
    if version > SCHEMA_VERSION {
        return Err(Error::Storage(format!(
            "database schema version {version} is newer than supported {SCHEMA_VERSION}"
        )));
    }
    if version < 1 {
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute_batch(SCHEMA_V1).map_err(db_err)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
    }
    Ok(())
}

/// Map rusqlite errors onto the crate error type.
fn db_err(e: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(ffi_err, msg) = &e {
        if ffi_err.code == ErrorCode::ConstraintViolation {
            let text = msg.clone().unwrap_or_else(|| e.to_string());
            match ffi_err.extended_code {
                rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => return Error::Conflict(text),
                rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                    return Error::NotFound("referenced row does not exist".into())
                }
                _ => {}
            }
        }
    }
    Error::Database(e.to_string())
}

fn format_ts(t: &DateTime<Utc>) -> String {
    t.format(TS_FORMAT).to_string()
}

fn parse_ts(s: &str) -> std::result::Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc))
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Internal(format!("serialise: {e}")))
}

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| Error::Invalid(format!("{what} out of range")))
}

fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
}

fn conv_err(idx: usize, msg: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, msg.to_string().into())
}

fn ts_col(r: &Row<'_>, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = r.get(idx)?;
    parse_ts(&s).map_err(|e| conv_err(idx, e))
}

fn opt_ts_col(r: &Row<'_>, idx: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let s: Option<String> = r.get(idx)?;
    s.map(|s| parse_ts(&s).map_err(|e| conv_err(idx, e)))
        .transpose()
}

fn json_col<T: DeserializeOwned>(r: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let s: String = r.get(idx)?;
    serde_json::from_str(&s).map_err(|e| conv_err(idx, e))
}

fn u64_col(r: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let v: i64 = r.get(idx)?;
    u64::try_from(v).map_err(|e| conv_err(idx, e))
}

fn enum_col<T>(
    r: &Row<'_>,
    idx: usize,
    parse: fn(&str) -> Option<T>,
    what: &str,
) -> rusqlite::Result<T> {
    let s: String = r.get(idx)?;
    parse(&s).ok_or_else(|| conv_err(idx, format!("unknown {what} '{s}'")))
}

fn camera_by_id(conn: &Connection, id: CameraId) -> Result<Camera> {
    conn.query_row(
        &format!("{SELECT_CAMERA} WHERE id = ?1"),
        params![id],
        camera_from_row,
    )
    .optional()
    .map_err(db_err)?
    .ok_or_else(|| Error::NotFound(format!("camera {id}")))
}

fn camera_from_row(r: &Row<'_>) -> rusqlite::Result<Camera> {
    Ok(Camera {
        id: r.get(0)?,
        name: r.get(1)?,
        enabled: r.get(2)?,
        source: json_col(r, 3)?,
        recording: json_col(r, 4)?,
        motion: json_col(r, 5)?,
        created_at: ts_col(r, 6)?,
        updated_at: ts_col(r, 7)?,
    })
}

fn recording_by_id(conn: &Connection, id: RecordingId) -> Result<Recording> {
    conn.query_row(
        &format!("{SELECT_RECORDING} WHERE id = ?1"),
        params![id],
        recording_from_row,
    )
    .optional()
    .map_err(db_err)?
    .ok_or_else(|| Error::NotFound(format!("recording {id}")))
}

fn recording_from_row(r: &Row<'_>) -> rusqlite::Result<Recording> {
    Ok(Recording {
        id: r.get(0)?,
        camera_id: r.get(1)?,
        path: r.get(2)?,
        start: ts_col(r, 3)?,
        end: opt_ts_col(r, 4)?,
        frames: u64_col(r, 5)?,
        size_bytes: u64_col(r, 6)?,
        trigger: enum_col(r, 7, RecordingTrigger::parse, "trigger")?,
        locked: r.get(8)?,
    })
}

fn event_by_id(conn: &Connection, id: EventId) -> Result<Event> {
    conn.query_row(
        &format!("{SELECT_EVENT} WHERE id = ?1"),
        params![id],
        event_from_row,
    )
    .optional()
    .map_err(db_err)?
    .ok_or_else(|| Error::NotFound(format!("event {id}")))
}

fn event_from_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    let score: f64 = r.get(4)?;
    Ok(Event {
        id: r.get(0)?,
        camera_id: r.get(1)?,
        kind: enum_col(r, 2, EventKind::parse, "event kind")?,
        at: ts_col(r, 3)?,
        score: score as f32,
        message: r.get(5)?,
        recording_id: r.get(6)?,
        acknowledged: r.get(7)?,
    })
}

fn user_by_id(conn: &Connection, id: UserId) -> Result<User> {
    conn.query_row(
        &format!("{SELECT_USER} WHERE id = ?1"),
        params![id],
        user_from_row,
    )
    .optional()
    .map_err(db_err)?
    .ok_or_else(|| Error::NotFound(format!("user {id}")))
}

fn user_from_row(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        password_hash: r.get(2)?,
        role: enum_col(r, 3, Role::parse, "role")?,
        created_at: ts_col(r, 4)?,
    })
}

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        token: r.get(0)?,
        user_id: r.get(1)?,
        created_at: ts_col(r, 2)?,
        expires_at: ts_col(r, 3)?,
    })
}
