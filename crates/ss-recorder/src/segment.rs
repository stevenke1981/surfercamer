//! Segment format: `<name>.mjpg` holds concatenated JPEG frames; `<name>.idx`
//! holds one 16-byte little-endian record per frame:
//! `u32 offset_ms` (since segment start), `u32 len`, `u64 byte_offset`.
//! Both are append-only so a crash leaves a readable prefix.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use ss_core::{CameraId, Error, Result};

pub const INDEX_RECORD_SIZE: usize = 16;

/// Index is flushed (together with data) at least this often, in frames.
const FLUSH_EVERY_FRAMES: u32 = 10;
/// Index is flushed (together with data) at least this often, in segment offset ms.
const FLUSH_EVERY_MS: u32 = 1_000;

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
    format!(
        "cam{}/{}/{}-{:03}.mjpg",
        camera_id,
        start.format("%Y-%m-%d"),
        start.format("%H%M%S"),
        start.timestamp_subsec_millis()
    )
}

/// Path of the index file for a `.mjpg` path.
pub fn index_path_for(data_path: &Path) -> std::path::PathBuf {
    data_path.with_extension("idx")
}

/// Join `rel_path` onto `root`, rejecting anything that could escape it.
pub(crate) fn safe_join(root: &Path, rel_path: &str) -> Result<PathBuf> {
    let rel = Path::new(rel_path);
    if rel_path.is_empty() {
        return Err(Error::Invalid("segment path is empty".into()));
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Invalid(format!("invalid segment path: {rel_path}")));
            }
        }
    }
    Ok(root.join(rel))
}

fn map_open_err(err: io::Error, path: &Path) -> Error {
    if err.kind() == io::ErrorKind::NotFound {
        Error::NotFound(path.display().to_string())
    } else {
        Error::Io(err)
    }
}

pub struct SegmentWriter {
    data: BufWriter<File>,
    index: BufWriter<File>,
    start: DateTime<Utc>,
    last_ts: DateTime<Utc>,
    frames: u64,
    data_bytes: u64,
    index_bytes: u64,
    unflushed_frames: u32,
    last_flush_offset_ms: u32,
}

impl SegmentWriter {
    /// Create `root/rel_path` (and parent dirs) plus its index file.
    pub fn create(root: &Path, rel_path: &str, start: DateTime<Utc>) -> Result<Self> {
        let data_path = safe_join(root, rel_path)?;
        if let Some(parent) = data_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data_file = File::options().write(true).create_new(true).open(&data_path)?;
        let index_file = File::options().write(true).create_new(true).open(index_path_for(&data_path))?;
        Ok(Self {
            data: BufWriter::new(data_file),
            index: BufWriter::new(index_file),
            start,
            last_ts: start,
            frames: 0,
            data_bytes: 0,
            index_bytes: 0,
            unflushed_frames: 0,
            last_flush_offset_ms: 0,
        })
    }

    /// Append one frame. `ts` earlier than start is clamped to offset 0.
    pub fn write_frame(&mut self, ts: DateTime<Utc>, jpeg: &[u8]) -> Result<()> {
        let len = u32::try_from(jpeg.len())
            .map_err(|_| Error::Invalid("frame larger than 4 GiB".into()))?;
        let offset_ms = u32::try_from((ts - self.start).num_milliseconds().clamp(0, i64::from(u32::MAX)))
            .unwrap_or(u32::MAX);
        let byte_offset = self.data_bytes;

        self.data.write_all(jpeg)?;
        self.data_bytes += u64::from(len);

        let mut record = [0u8; INDEX_RECORD_SIZE];
        record[0..4].copy_from_slice(&offset_ms.to_le_bytes());
        record[4..8].copy_from_slice(&len.to_le_bytes());
        record[8..16].copy_from_slice(&byte_offset.to_le_bytes());
        self.index.write_all(&record)?;
        self.index_bytes += INDEX_RECORD_SIZE as u64;

        self.frames += 1;
        self.last_ts = ts;
        self.unflushed_frames += 1;
        if self.unflushed_frames >= FLUSH_EVERY_FRAMES
            || offset_ms.saturating_sub(self.last_flush_offset_ms) >= FLUSH_EVERY_MS
        {
            self.checkpoint(offset_ms)?;
        }
        Ok(())
    }

    /// Flush data before index so every index record points at written data.
    fn checkpoint(&mut self, offset_ms: u32) -> Result<()> {
        self.data.flush()?;
        self.index.flush()?;
        self.unflushed_frames = 0;
        self.last_flush_offset_ms = offset_ms;
        Ok(())
    }

    pub fn start(&self) -> DateTime<Utc> {
        self.start
    }
    pub fn frames(&self) -> u64 {
        self.frames
    }
    /// Bytes of data + index written so far.
    pub fn size_bytes(&self) -> u64 {
        self.data_bytes + self.index_bytes
    }
    /// Timestamp of the last frame written (start if none).
    pub fn last_ts(&self) -> DateTime<Utc> {
        self.last_ts
    }

    /// Flush, fsync and close. `end` = timestamp of the last frame.
    pub fn finish(self) -> Result<SegmentSummary> {
        let size_bytes = self.size_bytes();
        let data = self.data.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        let index = self.index.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        data.sync_all()?;
        index.sync_all()?;
        Ok(SegmentSummary {
            start: self.start,
            end: self.last_ts,
            frames: self.frames,
            size_bytes,
        })
    }
}

pub struct SegmentReader {
    data: File,
    entries: Vec<IndexEntry>,
}

impl SegmentReader {
    /// Open a segment; ignores trailing partial index records and entries
    /// pointing beyond the data file's length.
    pub fn open(root: &Path, rel_path: &str) -> Result<Self> {
        let data_path = safe_join(root, rel_path)?;
        let index_path = index_path_for(&data_path);

        let data = File::open(&data_path).map_err(|e| map_open_err(e, &data_path))?;
        let data_len = data.metadata()?.len();
        let raw = fs::read(&index_path).map_err(|e| map_open_err(e, &index_path))?;

        let mut entries: Vec<IndexEntry> = Vec::with_capacity(raw.len() / INDEX_RECORD_SIZE);
        for rec in raw.chunks_exact(INDEX_RECORD_SIZE) {
            let entry = IndexEntry {
                offset_ms: u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]),
                len: u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]),
                byte_offset: u64::from_le_bytes([rec[8], rec[9], rec[10], rec[11], rec[12], rec[13], rec[14], rec[15]]),
            };
            let in_range = entry.byte_offset.checked_add(u64::from(entry.len)).is_some_and(|end| end <= data_len);
            // Offsets must be non-decreasing for `index_at`; drop from the first bad entry on.
            let ordered = entries.last().is_none_or(|prev| entry.offset_ms >= prev.offset_ms);
            if !in_range || !ordered {
                break;
            }
            entries.push(entry);
        }

        Ok(Self { data, entries })
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    pub fn duration_ms(&self) -> u32 {
        self.entries.last().map_or(0, |e| e.offset_ms)
    }

    pub fn read_frame(&mut self, index: usize) -> Result<Bytes> {
        let entry = *self
            .entries
            .get(index)
            .ok_or_else(|| Error::NotFound(format!("frame {index}")))?;
        self.data.seek(SeekFrom::Start(entry.byte_offset))?;
        let mut buf = vec![0u8; entry.len as usize];
        self.data.read_exact(&mut buf)?;
        Ok(Bytes::from(buf))
    }

    /// Index of the last frame with `offset_ms <= at_ms` (0 if before first).
    pub fn index_at(&self, at_ms: u32) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let count = self.entries.partition_point(|e| e.offset_ms <= at_ms);
        Some(count.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap()
    }

    fn frame(i: usize) -> Vec<u8> {
        // Varying lengths, distinct content per frame.
        (0..(100 + i * 7)).map(|b| (b as u8).wrapping_mul(i as u8 + 1).wrapping_add(i as u8)).collect()
    }

    fn write_n(root: &Path, rel: &str, n: usize, step_ms: i64) -> (SegmentSummary, Vec<Vec<u8>>) {
        let mut w = SegmentWriter::create(root, rel, t0()).unwrap();
        let mut frames = Vec::new();
        for i in 0..n {
            let f = frame(i);
            w.write_frame(t0() + Duration::milliseconds(i as i64 * step_ms), &f).unwrap();
            frames.push(f);
        }
        (w.finish().unwrap(), frames)
    }

    #[test]
    fn roundtrip_is_byte_identical_with_correct_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam1/2026-03-04/050607-000.mjpg";
        let n = 25;
        let (summary, frames) = write_n(dir.path(), rel, n, 100);

        let expected_size: u64 = frames.iter().map(|f| f.len() as u64).sum::<u64>() + (n * INDEX_RECORD_SIZE) as u64;
        assert_eq!(summary.frames, n as u64);
        assert_eq!(summary.size_bytes, expected_size);
        assert_eq!(summary.start, t0());
        assert_eq!(summary.end, t0() + Duration::milliseconds((n as i64 - 1) * 100));

        let mut r = SegmentReader::open(dir.path(), rel).unwrap();
        assert_eq!(r.entries().len(), n);
        assert_eq!(r.duration_ms(), ((n - 1) * 100) as u32);
        let mut byte_off = 0u64;
        for (i, e) in r.entries().iter().enumerate() {
            assert_eq!(e.offset_ms, (i * 100) as u32);
            assert_eq!(e.len as usize, frames[i].len());
            assert_eq!(e.byte_offset, byte_off);
            byte_off += e.len as u64;
        }
        // Read in reverse order to exercise seeking.
        for i in (0..n).rev() {
            assert_eq!(r.read_frame(i).unwrap().as_ref(), frames[i].as_slice());
        }
        assert!(matches!(r.read_frame(n), Err(Error::NotFound(_))));
    }

    #[test]
    fn index_at_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam2/2026-03-04/050607-000.mjpg";
        write_n(dir.path(), rel, 4, 100); // offsets 0, 100, 200, 300
        let r = SegmentReader::open(dir.path(), rel).unwrap();
        assert_eq!(r.index_at(0), Some(0));
        assert_eq!(r.index_at(50), Some(0));
        assert_eq!(r.index_at(100), Some(1));
        assert_eq!(r.index_at(299), Some(2));
        assert_eq!(r.index_at(300), Some(3));
        assert_eq!(r.index_at(u32::MAX), Some(3));
    }

    #[test]
    fn index_at_before_first_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Segment whose first frame arrives 500 ms after start.
        let rel = "cam3/2026-03-04/050607-000.mjpg";
        let mut w = SegmentWriter::create(dir.path(), rel, t0()).unwrap();
        w.write_frame(t0() + Duration::milliseconds(500), &frame(0)).unwrap();
        w.write_frame(t0() + Duration::milliseconds(600), &frame(1)).unwrap();
        w.finish().unwrap();
        let r = SegmentReader::open(dir.path(), rel).unwrap();
        assert_eq!(r.index_at(0), Some(0));
        assert_eq!(r.index_at(550), Some(0));
        assert_eq!(r.index_at(600), Some(1));

        // Empty segment.
        let rel_empty = "cam3/2026-03-04/050608-000.mjpg";
        let summary = SegmentWriter::create(dir.path(), rel_empty, t0()).unwrap().finish().unwrap();
        assert_eq!(summary.frames, 0);
        assert_eq!(summary.end, t0());
        assert_eq!(summary.size_bytes, 0);
        let re = SegmentReader::open(dir.path(), rel_empty).unwrap();
        assert!(re.entries().is_empty());
        assert_eq!(re.duration_ms(), 0);
        assert_eq!(re.index_at(0), None);
    }

    #[test]
    fn partial_segment_is_readable_before_finish() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam8/2026-03-04/050607-000.mjpg";
        let mut w = SegmentWriter::create(dir.path(), rel, t0()).unwrap();
        for i in 0..FLUSH_EVERY_FRAMES as usize {
            w.write_frame(t0() + Duration::milliseconds(i as i64), &frame(i)).unwrap();
        }
        // Writer still alive (no finish): the checkpoint must have reached disk.
        let r = SegmentReader::open(dir.path(), rel).unwrap();
        assert_eq!(r.entries().len(), FLUSH_EVERY_FRAMES as usize);
        assert_eq!(r.entries().len(), w.frames() as usize);
        drop(w);
    }

    #[test]
    fn frame_before_start_is_clamped_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam4/2026-03-04/050607-000.mjpg";
        let mut w = SegmentWriter::create(dir.path(), rel, t0()).unwrap();
        w.write_frame(t0() - Duration::seconds(3), &frame(0)).unwrap();
        w.finish().unwrap();
        let r = SegmentReader::open(dir.path(), rel).unwrap();
        assert_eq!(r.entries()[0].offset_ms, 0);
    }

    #[test]
    fn reader_tolerates_partial_trailing_index_record() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam5/2026-03-04/050607-000.mjpg";
        let (_, frames) = write_n(dir.path(), rel, 5, 100);
        let idx = index_path_for(&dir.path().join(rel));
        let len = fs::metadata(&idx).unwrap().len();
        let f = File::options().write(true).open(&idx).unwrap();
        f.set_len(len - 5).unwrap(); // last record now partial

        let mut r = SegmentReader::open(dir.path(), rel).unwrap();
        assert_eq!(r.entries().len(), 4);
        assert_eq!(r.read_frame(3).unwrap().as_ref(), frames[3].as_slice());
        assert!(r.read_frame(4).is_err());
    }

    #[test]
    fn reader_drops_entries_beyond_truncated_data() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam6/2026-03-04/050607-000.mjpg";
        let (_, frames) = write_n(dir.path(), rel, 10, 100);
        let data = dir.path().join(rel);
        let keep: u64 = frames[..6].iter().map(|f| f.len() as u64).sum::<u64>() + 3; // cut into frame 6
        File::options().write(true).open(&data).unwrap().set_len(keep).unwrap();

        let mut r = SegmentReader::open(dir.path(), rel).unwrap();
        assert_eq!(r.entries().len(), 6);
        assert_eq!(r.duration_ms(), 500);
        for (i, f) in frames.iter().enumerate().take(6) {
            assert_eq!(r.read_frame(i).unwrap().as_ref(), f.as_slice());
        }
    }

    #[test]
    fn open_missing_segment_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            SegmentReader::open(dir.path(), "cam9/2026-03-04/000000-000.mjpg"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn path_traversal_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["../evil.mjpg", "/abs/evil.mjpg", "cam1/../../evil.mjpg", ""] {
            assert!(
                matches!(SegmentWriter::create(dir.path(), bad, t0()), Err(Error::Invalid(_))),
                "create accepted {bad:?}"
            );
            assert!(
                matches!(SegmentReader::open(dir.path(), bad), Err(Error::Invalid(_))),
                "open accepted {bad:?}"
            );
        }
        assert!(!dir.path().parent().unwrap().join("evil.mjpg").exists());
    }

    #[test]
    fn create_makes_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam7/2026-03-04/050607-123.mjpg";
        SegmentWriter::create(dir.path(), rel, t0()).unwrap().finish().unwrap();
        assert!(dir.path().join("cam7/2026-03-04/050607-123.mjpg").is_file());
        assert!(dir.path().join("cam7/2026-03-04/050607-123.idx").is_file());
    }

    #[test]
    fn segment_rel_path_format() {
        let start = t0() + Duration::milliseconds(45);
        assert_eq!(segment_rel_path(7, start), "cam7/2026-03-04/050607-045.mjpg");
        let late = Utc.with_ymd_and_hms(2027, 12, 31, 23, 59, 59).unwrap() + Duration::milliseconds(999);
        assert_eq!(segment_rel_path(120, late), "cam120/2027-12-31/235959-999.mjpg");
    }
}
