//! Segment format: `<name>.mjpg` holds concatenated JPEG frames; `<name>.idx`
//! holds one 16-byte little-endian record per frame:
//! `u32 offset_ms` (since segment start), `u32 len`, `u64 byte_offset`.
//! Both are append-only so a crash leaves a readable prefix.

use std::path::Path;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use ss_core::{CameraId, Result};

pub const INDEX_RECORD_SIZE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub offset_ms: u32,
    pub len: u32,
    pub byte_offset: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentSummary {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub frames: u64,
    pub size_bytes: u64,
}

/// Relative path for a new segment:
/// `cam{camera_id}/{YYYY-MM-DD}/{HHMMSS}-{millis:03}.mjpg` (UTC).
pub fn segment_rel_path(camera_id: CameraId, start: DateTime<Utc>) -> String {
    let _ = (camera_id, start);
    todo!()
}

/// Path of the index file for a `.mjpg` path.
pub fn index_path_for(data_path: &Path) -> std::path::PathBuf {
    data_path.with_extension("idx")
}

pub struct SegmentWriter {
    _priv: (),
}

impl SegmentWriter {
    /// Create `root/rel_path` (and parent dirs) plus its index file.
    pub fn create(root: &Path, rel_path: &str, start: DateTime<Utc>) -> Result<Self> {
        let _ = (root, rel_path, start);
        todo!()
    }

    /// Append one frame. `ts` earlier than start is clamped to offset 0.
    pub fn write_frame(&mut self, ts: DateTime<Utc>, jpeg: &[u8]) -> Result<()> {
        let _ = (ts, jpeg);
        todo!()
    }

    pub fn start(&self) -> DateTime<Utc> {
        todo!()
    }
    pub fn frames(&self) -> u64 {
        todo!()
    }
    /// Bytes of data + index written so far.
    pub fn size_bytes(&self) -> u64 {
        todo!()
    }
    /// Timestamp of the last frame written (start if none).
    pub fn last_ts(&self) -> DateTime<Utc> {
        todo!()
    }

    /// Flush, fsync and close. `end` = timestamp of the last frame.
    pub fn finish(self) -> Result<SegmentSummary> {
        todo!()
    }
}

pub struct SegmentReader {
    _priv: (),
}

impl SegmentReader {
    /// Open a segment; ignores trailing partial index records and entries
    /// pointing beyond the data file's length.
    pub fn open(root: &Path, rel_path: &str) -> Result<Self> {
        let _ = (root, rel_path);
        todo!()
    }

    pub fn entries(&self) -> &[IndexEntry] {
        todo!()
    }

    pub fn duration_ms(&self) -> u32 {
        todo!()
    }

    pub fn read_frame(&mut self, index: usize) -> Result<Bytes> {
        let _ = index;
        todo!()
    }

    /// Index of the last frame with `offset_ms <= at_ms` (0 if before first).
    pub fn index_at(&self, at_ms: u32) -> Option<usize> {
        let _ = at_ms;
        todo!()
    }
}
