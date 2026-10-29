//! Retention planning (pure) and file deletion.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use ss_core::{Error, Recording, RecordingId, Result, RetentionPolicy};

use crate::segment::{index_path_for, safe_join};

/// Pick recordings to delete. `candidates` are unlocked finished recordings,
/// oldest first; `usage` is the camera's total bytes (including locked).
/// Rules: delete all with `end < now - max_days`; then, while
/// `usage - deleted_bytes > max_bytes`, delete the oldest remaining.
pub fn plan(
    candidates: &[Recording],
    policy: &RetentionPolicy,
    usage: u64,
    now: DateTime<Utc>,
) -> Vec<RecordingId> {
    // Defensive filtering: no duplicates, never locked or unfinished.
    let mut seen = HashSet::new();
    let eligible: Vec<&Recording> = candidates
        .iter()
        .filter(|r| !r.locked && r.end.is_some() && seen.insert(r.id))
        .collect();

    let mut chosen = vec![false; eligible.len()];
    let mut freed: u64 = 0;

    // (a) age
    if let Some(days) = policy.max_days {
        let cutoff = Duration::try_days(i64::from(days)).and_then(|d| now.checked_sub_signed(d));
        if let Some(cutoff) = cutoff {
            for (i, r) in eligible.iter().enumerate() {
                if r.end.is_some_and(|end| end < cutoff) {
                    chosen[i] = true;
                    freed = freed.saturating_add(r.size_bytes);
                }
            }
        }
    }

    // (b) size, oldest first
    if let Some(max_bytes) = policy.max_bytes {
        let mut remaining = usage.saturating_sub(freed);
        for (i, r) in eligible.iter().enumerate() {
            if remaining <= max_bytes {
                break;
            }
            if !chosen[i] {
                chosen[i] = true;
                remaining = remaining.saturating_sub(r.size_bytes);
            }
        }
    }

    let mut picked: Vec<&Recording> = eligible
        .iter()
        .zip(&chosen)
        .filter(|(_, &c)| c)
        .map(|(r, _)| *r)
        .collect();
    picked.sort_by_key(|r| (r.start, r.id));
    picked.into_iter().map(|r| r.id).collect()
}

/// Delete a segment's `.mjpg` and `.idx`; missing files are not an error.
/// Also removes the parent day directory if it became empty.
pub fn delete_segment_files(root: &Path, rel_path: &str) -> Result<()> {
    let data_path = safe_join(root, rel_path)?;
    remove_file_if_exists(&data_path)?;
    remove_file_if_exists(&index_path_for(&data_path))?;
    if let Some(parent) = data_path.parent() {
        if parent != root && parent.starts_with(root) {
            remove_dir_if_empty(parent)?;
        }
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

fn remove_dir_if_empty(dir: &Path) -> Result<()> {
    let mut entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    if entries.next().is_some() {
        return Ok(());
    }
    match fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        // Raced with another writer: not empty any more, or already gone.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ss_core::RecordingTrigger;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap()
    }

    /// Recording that ended `days_ago` days before `now()`.
    fn rec(id: i64, days_ago: i64, size: u64) -> Recording {
        let end = now() - Duration::days(days_ago);
        Recording {
            id,
            camera_id: 1,
            path: format!("cam1/x/{id}.mjpg"),
            start: end - Duration::minutes(5),
            end: Some(end),
            frames: 10,
            size_bytes: size,
            trigger: RecordingTrigger::Continuous,
            locked: false,
        }
    }

    /// Candidates oldest first (larger days_ago = older).
    fn sample() -> Vec<Recording> {
        vec![
            rec(1, 40, 100),
            rec(2, 20, 100),
            rec(3, 10, 100),
            rec(4, 1, 100),
        ]
    }

    fn policy(max_days: Option<u32>, max_bytes: Option<u64>) -> RetentionPolicy {
        RetentionPolicy {
            max_days,
            max_bytes,
        }
    }

    #[test]
    fn age_only() {
        let ids = plan(&sample(), &policy(Some(15), None), 400, now());
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn size_only_deletes_oldest_until_under_limit() {
        // usage 400, limit 250 -> delete 1 (300), delete 2 (200) stops.
        let ids = plan(&sample(), &policy(None, Some(250)), 400, now());
        assert_eq!(ids, vec![1, 2]);
        // usage 400, limit 400 -> nothing.
        assert!(plan(&sample(), &policy(None, Some(400)), 400, now()).is_empty());
    }

    #[test]
    fn no_policy_deletes_nothing() {
        assert!(plan(&sample(), &policy(None, None), u64::MAX, now()).is_empty());
    }

    #[test]
    fn age_deletions_count_towards_size_limit() {
        // Age removes ids 1 and 2 (200 bytes); usage 400 - 200 = 200 <= 250, so no size deletions.
        let ids = plan(&sample(), &policy(Some(15), Some(250)), 400, now());
        assert_eq!(ids, vec![1, 2]);
        // Limit 100: after age, remaining 200 > 100 -> delete id 3 too.
        let ids = plan(&sample(), &policy(Some(15), Some(100)), 400, now());
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn locked_and_unfinished_are_skipped() {
        let mut cands = sample();
        cands[0].locked = true; // oldest, locked
        cands[1].end = None; // unfinished
        let ids = plan(&cands, &policy(Some(15), Some(0)), 400, now());
        assert_eq!(ids, vec![3, 4]);
    }

    #[test]
    fn no_duplicates_even_if_candidates_repeat() {
        let mut cands = sample();
        cands.push(cands[0].clone());
        let ids = plan(&cands, &policy(Some(15), None), 500, now());
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn output_is_oldest_first() {
        // Candidates deliberately listed newest first; output must still be oldest first.
        let mut cands = sample();
        cands.reverse();
        let ids = plan(&cands, &policy(Some(15), None), 400, now());
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn huge_max_days_does_not_panic() {
        assert!(plan(&sample(), &policy(Some(u32::MAX), None), 400, now()).is_empty());
    }

    fn make_segment(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"data").unwrap();
        fs::write(index_path_for(&p), b"idx").unwrap();
    }

    #[test]
    fn delete_removes_both_files_and_empty_parent() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "cam1/2026-06-30/120000-000.mjpg";
        make_segment(dir.path(), rel);
        delete_segment_files(dir.path(), rel).unwrap();
        assert!(!dir.path().join(rel).exists());
        assert!(!dir.path().join("cam1/2026-06-30/120000-000.idx").exists());
        assert!(!dir.path().join("cam1/2026-06-30").exists());
        assert!(dir.path().exists());
    }

    #[test]
    fn delete_keeps_parent_with_other_files() {
        let dir = tempfile::tempdir().unwrap();
        make_segment(dir.path(), "cam1/2026-06-30/120000-000.mjpg");
        make_segment(dir.path(), "cam1/2026-06-30/120500-000.mjpg");
        delete_segment_files(dir.path(), "cam1/2026-06-30/120000-000.mjpg").unwrap();
        assert!(dir.path().join("cam1/2026-06-30/120500-000.mjpg").exists());
    }

    #[test]
    fn delete_tolerates_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        delete_segment_files(dir.path(), "cam2/2026-06-30/000000-000.mjpg").unwrap();
        make_segment(dir.path(), "cam2/2026-06-30/000001-000.mjpg");
        // Only the data file is present.
        fs::remove_file(dir.path().join("cam2/2026-06-30/000001-000.idx")).unwrap();
        delete_segment_files(dir.path(), "cam2/2026-06-30/000001-000.mjpg").unwrap();
        assert!(!dir.path().join("cam2/2026-06-30").exists());
    }

    #[test]
    fn delete_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            delete_segment_files(dir.path(), "../x.mjpg"),
            Err(Error::Invalid(_))
        ));
    }
}
