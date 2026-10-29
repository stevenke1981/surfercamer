//! Filesystem capacity.

use std::path::Path;

use ss_core::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DiskSpace {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Capacity of the filesystem containing `path` (statvfs on Unix).
#[cfg(unix)]
pub fn disk_space(path: &Path) -> Result<DiskSpace> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    use ss_core::Error;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Invalid("path contains a NUL byte".into()))?;
    // SAFETY: statvfs only writes into the struct we pass; zeroed is a valid start value.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and st is a valid out-pointer.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    let frsize = st.f_frsize as u64;
    Ok(DiskSpace {
        total_bytes: (st.f_blocks as u64).saturating_mul(frsize),
        free_bytes: (st.f_bavail as u64).saturating_mul(frsize),
    })
}

/// Capacity of the filesystem containing `path` (statvfs on Unix).
/// Off Unix there is no portable query, so both values are reported as unlimited.
#[cfg(not(unix))]
pub fn disk_space(path: &Path) -> Result<DiskSpace> {
    let _ = path;
    Ok(DiskSpace {
        total_bytes: u64::MAX,
        free_bytes: u64::MAX,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempdir_capacity_is_sane() {
        let dir = tempfile::tempdir().unwrap();
        let space = disk_space(dir.path()).unwrap();
        assert!(space.total_bytes >= space.free_bytes);
        assert!(space.free_bytes > 0);
    }

    #[cfg(unix)]
    #[test]
    fn missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(disk_space(&dir.path().join("does-not-exist")).is_err());
    }
}
