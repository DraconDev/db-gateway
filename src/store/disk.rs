//! How much room is left on the filesystem that holds the data.
//!
//! The observability contract names free bytes and free inodes for the data
//! directory as the disk-pressure signal: low disk blocks safe writes,
//! imports and staging, and an operator wants to see that coming rather
//! than read about it afterwards in a failed write.
//!
//! This is the filesystem's own accounting, not the gateway's: the
//! directories the gateway writes are a small part of the volume, and the
//! part that matters for pressure is everything else on it too.

use std::path::Path;

/// Free and total space of the filesystem that holds a path.
///
/// `free_bytes` is the space available to an unprivileged writer
/// (`f_bavail`), not the space free of all reservations (`f_bfree`): the
/// reserved blocks belong to the system, and promising them to an
/// operator would overstate the room there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    /// The size of the filesystem.
    pub total_bytes: u64,
    /// The space an unprivileged writer may still use.
    pub free_bytes: u64,
    /// The space in use, including the system's reserved blocks.
    pub used_bytes: u64,
    /// The inodes the filesystem has.
    pub total_inodes: u64,
    /// The inodes still free.
    pub free_inodes: u64,
}

impl DiskSpace {
    /// The fraction of the filesystem that is in use, as a percentage.
    pub fn used_percent(&self) -> u64 {
        if self.total_bytes == 0 {
            return 0;
        }
        self.used_bytes.saturating_mul(100) / self.total_bytes
    }
}

/// The space on the filesystem that holds `path`, if it can be read.
///
/// `None` when the platform cannot answer, or the path does not exist:
/// an unknown amount of room is reported as unknown, never as a number
/// that would read like a measurement.
#[cfg(unix)]
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let text = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` writes into the `statvfs` it is given and reads
    // nothing from it first. The struct is plain data with no padding
    // that matters here, and the path is a valid NUL-terminated string
    // that outlives the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(text.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64;
    let free = stat.f_bfree as u64;
    Some(DiskSpace {
        total_bytes: total.saturating_mul(block),
        free_bytes: (stat.f_bavail as u64).saturating_mul(block),
        used_bytes: total.saturating_sub(free).saturating_mul(block),
        total_inodes: stat.f_files as u64,
        free_inodes: stat.f_favail as u64,
    })
}

/// The space on the filesystem that holds `path`, if it can be read.
///
/// Windows has no `statvfs`, so the answer there is always unknown.
#[cfg(not(unix))]
pub fn disk_space(_path: &Path) -> Option<DiskSpace> {
    None
}
