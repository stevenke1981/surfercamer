use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::model::*;

/// Persistence contract, implemented by `ss_db::SqliteStore`.
///
/// Methods are synchronous (SQLite is local and fast); async callers wrap
/// heavy calls in `spawn_blocking` if needed. `NotFound` is returned for
/// missing ids; `Conflict` for unique-constraint violations.
pub trait Store: Send + Sync + 'static {
    // cameras
    fn create_camera(&self, input: &CameraInput) -> Result<Camera>;
    fn get_camera(&self, id: CameraId) -> Result<Camera>;
    fn list_cameras(&self) -> Result<Vec<Camera>>;
    fn update_camera(&self, id: CameraId, input: &CameraInput) -> Result<Camera>;
    /// Deletes the camera and its recording/event rows (files are the caller's job).
    fn delete_camera(&self, id: CameraId) -> Result<()>;

    // recordings
    fn create_recording(&self, rec: &NewRecording) -> Result<Recording>;
    fn finish_recording(&self, id: RecordingId, end: DateTime<Utc>, frames: u64, size_bytes: u64) -> Result<Recording>;
    fn get_recording(&self, id: RecordingId) -> Result<Recording>;
    /// Ordered by `start` descending.
    fn list_recordings(&self, q: &RecordingQuery) -> Result<Vec<Recording>>;
    fn set_recording_locked(&self, id: RecordingId, locked: bool) -> Result<Recording>;
    fn delete_recording(&self, id: RecordingId) -> Result<()>;
    /// Sum of `size_bytes` for a camera (all cameras if `None`).
    fn recording_usage(&self, camera_id: Option<CameraId>) -> Result<u64>;
    /// Unlocked, finished recordings of a camera, oldest first.
    fn retention_candidates(&self, camera_id: CameraId) -> Result<Vec<Recording>>;

    // events
    fn create_event(&self, ev: &NewEvent) -> Result<Event>;
    /// Ordered by `at` descending.
    fn list_events(&self, q: &EventQuery) -> Result<Vec<Event>>;
    fn acknowledge_event(&self, id: EventId) -> Result<Event>;
    /// Attach an event to a recording (e.g. the motion segment it started).
    fn set_event_recording(&self, id: EventId, recording_id: RecordingId) -> Result<()>;
    fn delete_events_before(&self, before: DateTime<Utc>) -> Result<u64>;

    // users & sessions
    fn create_user(&self, username: &str, password_hash: &str, role: Role) -> Result<User>;
    fn get_user(&self, id: UserId) -> Result<User>;
    fn get_user_by_name(&self, username: &str) -> Result<User>;
    fn list_users(&self) -> Result<Vec<User>>;
    /// `None` fields are left unchanged.
    fn update_user(&self, id: UserId, password_hash: Option<&str>, role: Option<Role>) -> Result<User>;
    fn delete_user(&self, id: UserId) -> Result<()>;
    fn count_users(&self) -> Result<u64>;

    fn create_session(&self, session: &Session) -> Result<()>;
    /// Returns `NotFound` if missing or expired.
    fn get_session(&self, token: &str) -> Result<Session>;
    fn delete_session(&self, token: &str) -> Result<()>;
    /// Revoke all sessions of a user except `except_token`; returns rows deleted.
    fn delete_sessions_for_user(&self, user_id: UserId, except_token: Option<&str>) -> Result<u64>;
    fn purge_expired_sessions(&self, now: DateTime<Utc>) -> Result<u64>;

    // settings
    fn get_settings(&self) -> Result<Settings>;
    fn put_settings(&self, settings: &Settings) -> Result<()>;
}
