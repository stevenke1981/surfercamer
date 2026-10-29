//! SQLite implementation of [`ss_core::Store`]. See docs/SPEC.md §5.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use ss_core::*;

/// Thread-safe SQLite store. One connection behind a mutex; WAL mode.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) the database file and run migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let _ = path;
        todo!()
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self> {
        todo!()
    }
}

impl Store for SqliteStore {
    fn create_camera(&self, input: &CameraInput) -> Result<Camera> { todo!() }
    fn get_camera(&self, id: CameraId) -> Result<Camera> { todo!() }
    fn list_cameras(&self) -> Result<Vec<Camera>> { todo!() }
    fn update_camera(&self, id: CameraId, input: &CameraInput) -> Result<Camera> { todo!() }
    fn delete_camera(&self, id: CameraId) -> Result<()> { todo!() }
    fn create_recording(&self, rec: &NewRecording) -> Result<Recording> { todo!() }
    fn finish_recording(&self, id: RecordingId, end: DateTime<Utc>, frames: u64, size_bytes: u64) -> Result<Recording> { todo!() }
    fn get_recording(&self, id: RecordingId) -> Result<Recording> { todo!() }
    fn list_recordings(&self, q: &RecordingQuery) -> Result<Vec<Recording>> { todo!() }
    fn set_recording_locked(&self, id: RecordingId, locked: bool) -> Result<Recording> { todo!() }
    fn delete_recording(&self, id: RecordingId) -> Result<()> { todo!() }
    fn recording_usage(&self, camera_id: Option<CameraId>) -> Result<u64> { todo!() }
    fn retention_candidates(&self, camera_id: CameraId) -> Result<Vec<Recording>> { todo!() }
    fn create_event(&self, ev: &NewEvent) -> Result<Event> { todo!() }
    fn list_events(&self, q: &EventQuery) -> Result<Vec<Event>> { todo!() }
    fn acknowledge_event(&self, id: EventId) -> Result<Event> { todo!() }
    fn delete_events_before(&self, before: DateTime<Utc>) -> Result<u64> { todo!() }
    fn create_user(&self, username: &str, password_hash: &str, role: Role) -> Result<User> { todo!() }
    fn get_user(&self, id: UserId) -> Result<User> { todo!() }
    fn get_user_by_name(&self, username: &str) -> Result<User> { todo!() }
    fn list_users(&self) -> Result<Vec<User>> { todo!() }
    fn update_user(&self, id: UserId, password_hash: Option<&str>, role: Option<Role>) -> Result<User> { todo!() }
    fn delete_user(&self, id: UserId) -> Result<()> { todo!() }
    fn count_users(&self) -> Result<u64> { todo!() }
    fn create_session(&self, session: &Session) -> Result<()> { todo!() }
    fn get_session(&self, token: &str) -> Result<Session> { todo!() }
    fn delete_session(&self, token: &str) -> Result<()> { todo!() }
    fn purge_expired_sessions(&self, now: DateTime<Utc>) -> Result<u64> { todo!() }
    fn get_settings(&self) -> Result<Settings> { todo!() }
    fn put_settings(&self, settings: &Settings) -> Result<()> { todo!() }
}
