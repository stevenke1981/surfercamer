//! Filesystem capacity.

use std::path::Path;

use ss_core::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DiskSpace {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Capacity of the filesystem containing `path` (statvfs on Unix).
pub fn disk_space(path: &Path) -> Result<DiskSpace> {
    let _ = path;
    todo!()
}
