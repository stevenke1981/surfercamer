//! Retention planning (pure) and file deletion.

use std::path::Path;

use chrono::{DateTime, Utc};
use ss_core::{Recording, RecordingId, Result, RetentionPolicy};

/// Pick recordings to delete. `candidates` are unlocked finished recordings,
/// oldest first; `usage` is the camera's total bytes (including locked).
/// Rules: delete all with `end < now - max_days`; then, while
/// `usage - deleted_bytes > max_bytes`, delete the oldest remaining.
pub fn plan(candidates: &[Recording], policy: &RetentionPolicy, usage: u64, now: DateTime<Utc>) -> Vec<RecordingId> {
    let _ = (candidates, policy, usage, now);
    todo!()
}

/// Delete a segment's `.mjpg` and `.idx`; missing files are not an error.
/// Also removes the parent day directory if it became empty.
pub fn delete_segment_files(root: &Path, rel_path: &str) -> Result<()> {
    let _ = (root, rel_path);
    todo!()
}
