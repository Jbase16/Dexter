use std::{
    ffi::CString,
    io,
    path::{Path, PathBuf},
};

use tracing::{error, info, warn};

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;
const BYTES_PER_MIB: u64 = 1024 * 1024;

/// Warn before normal Rust/Swift build artifacts can exhaust the boot volume.
///
/// This is intentionally conservative: Dexter recently hit "No space left on
/// device" with only a few hundred MiB free after debug artifacts rebuilt. Two
/// GiB gives the operator time to clean up before worker logs, session state,
/// SQLite writes, or a debug build trip over the same failure class.
pub(crate) const DISK_WARN_AVAILABLE_BYTES: u64 = 2 * BYTES_PER_GIB;

/// Mark disk space critical when routine writes can fail immediately.
///
/// At this level even small rebuilds, SQLite writes, Python worker caches, and
/// session-state persistence become unreliable. Dexter still starts because the
/// operator may need diagnostics, but health reports this as a failed check.
pub(crate) const DISK_CRITICAL_AVAILABLE_BYTES: u64 = 512 * BYTES_PER_MIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiskStatus {
    Ready,
    Warn,
    Critical,
    Unavailable,
}

impl DiskStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Warn => "warn",
            Self::Critical => "critical",
            Self::Unavailable => "unavailable",
        }
    }

    pub(crate) fn is_ready(self) -> bool {
        self == Self::Ready
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskHealthSnapshot {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) status: DiskStatus,
    pub(crate) available_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) detail: String,
}

impl DiskHealthSnapshot {
    pub(crate) fn probe(name: impl Into<String>, path: impl AsRef<Path>) -> Self {
        let name = name.into();
        let requested_path = path.as_ref();
        let probe_path = nearest_existing_path(requested_path);

        match statvfs_usage(&probe_path) {
            Ok(usage) => {
                let status = classify_available_bytes(usage.available_bytes);
                let detail = match status {
                    DiskStatus::Ready => {
                        format!("{} available", format_bytes_gib(usage.available_bytes))
                    }
                    DiskStatus::Warn => format!(
                        "{} available; below {} warning threshold",
                        format_bytes_gib(usage.available_bytes),
                        format_bytes_gib(DISK_WARN_AVAILABLE_BYTES)
                    ),
                    DiskStatus::Critical => format!(
                        "{} available; below {} critical threshold",
                        format_bytes_gib(usage.available_bytes),
                        format_bytes_gib(DISK_CRITICAL_AVAILABLE_BYTES)
                    ),
                    DiskStatus::Unavailable => "disk usage unavailable".to_string(),
                };

                Self {
                    name,
                    path: requested_path.display().to_string(),
                    status,
                    available_bytes: usage.available_bytes,
                    total_bytes: usage.total_bytes,
                    detail,
                }
            }
            Err(error) => Self {
                name,
                path: requested_path.display().to_string(),
                status: DiskStatus::Unavailable,
                available_bytes: 0,
                total_bytes: 0,
                detail: format!(
                    "could not inspect {} via {}: {error}",
                    requested_path.display(),
                    probe_path.display()
                ),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskUsage {
    available_bytes: u64,
    total_bytes: u64,
}

pub(crate) fn collect_operator_disk_health(state_dir: &Path) -> Vec<DiskHealthSnapshot> {
    let mut snapshots = Vec::with_capacity(3);
    snapshots.push(DiskHealthSnapshot::probe("state", state_dir));

    if let Ok(cwd) = std::env::current_dir() {
        snapshots.push(DiskHealthSnapshot::probe("workspace", cwd));
    }

    snapshots.push(DiskHealthSnapshot::probe("temp", std::env::temp_dir()));
    snapshots
}

pub(crate) fn disk_degraded_components(disks: &[DiskHealthSnapshot]) -> Vec<String> {
    disks
        .iter()
        .filter(|disk| !disk.status.is_ready())
        .map(|disk| format!("disk:{}", disk.name))
        .collect()
}

pub(crate) fn disk_degraded_label(disks: &[DiskHealthSnapshot]) -> String {
    let components = disk_degraded_components(disks);
    if components.is_empty() {
        "none".to_string()
    } else {
        components.join(",")
    }
}

#[allow(dead_code)] // Used by dexter-core; unused when this module is included by dexter-cli.
pub(crate) fn log_disk_health(snapshots: &[DiskHealthSnapshot]) {
    for disk in snapshots {
        match disk.status {
            DiskStatus::Ready => {
                info!(
                    component = %disk.name,
                    path = %disk.path,
                    status = disk.status.as_str(),
                    available_bytes = disk.available_bytes,
                    total_bytes = disk.total_bytes,
                    detail = %disk.detail,
                    "Disk health check ok"
                );
            }
            DiskStatus::Warn => {
                warn!(
                    component = %disk.name,
                    path = %disk.path,
                    status = disk.status.as_str(),
                    available_bytes = disk.available_bytes,
                    total_bytes = disk.total_bytes,
                    detail = %disk.detail,
                    "Disk health check warning"
                );
            }
            DiskStatus::Critical => {
                error!(
                    component = %disk.name,
                    path = %disk.path,
                    status = disk.status.as_str(),
                    available_bytes = disk.available_bytes,
                    total_bytes = disk.total_bytes,
                    detail = %disk.detail,
                    "Disk health check critical"
                );
            }
            DiskStatus::Unavailable => {
                warn!(
                    component = %disk.name,
                    path = %disk.path,
                    status = disk.status.as_str(),
                    detail = %disk.detail,
                    "Disk health check unavailable"
                );
            }
        }
    }
}

pub(crate) fn format_bytes_gib(bytes: u64) -> String {
    let gib = bytes as f64 / BYTES_PER_GIB as f64;
    format!("{gib:.1} GiB")
}

fn classify_available_bytes(available_bytes: u64) -> DiskStatus {
    if available_bytes < DISK_CRITICAL_AVAILABLE_BYTES {
        DiskStatus::Critical
    } else if available_bytes < DISK_WARN_AVAILABLE_BYTES {
        DiskStatus::Warn
    } else {
        DiskStatus::Ready
    }
}

fn nearest_existing_path(path: &Path) -> PathBuf {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return candidate.to_path_buf();
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return PathBuf::from("/"),
        }
    }
}

#[cfg(unix)]
fn statvfs_usage(path: &Path) -> io::Result<DiskUsage> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL byte",
        )
    })?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    } as u128;
    let available = (stat.f_bavail as u128).saturating_mul(block_size);
    let total = (stat.f_blocks as u128).saturating_mul(block_size);

    Ok(DiskUsage {
        available_bytes: available.min(u64::MAX as u128) as u64,
        total_bytes: total.min(u64::MAX as u128) as u64,
    })
}

#[cfg(not(unix))]
fn statvfs_usage(_path: &Path) -> io::Result<DiskUsage> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "disk health probing requires Unix statvfs",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_available_bytes_maps_thresholds() {
        assert_eq!(
            classify_available_bytes(DISK_WARN_AVAILABLE_BYTES),
            DiskStatus::Ready
        );
        assert_eq!(
            classify_available_bytes(DISK_WARN_AVAILABLE_BYTES - 1),
            DiskStatus::Warn
        );
        assert_eq!(
            classify_available_bytes(DISK_CRITICAL_AVAILABLE_BYTES),
            DiskStatus::Warn
        );
        assert_eq!(
            classify_available_bytes(DISK_CRITICAL_AVAILABLE_BYTES - 1),
            DiskStatus::Critical
        );
    }

    #[test]
    fn disk_degraded_components_names_non_ready_disks() {
        let disks = vec![
            DiskHealthSnapshot {
                name: "state".to_string(),
                path: "/tmp/state".to_string(),
                status: DiskStatus::Ready,
                available_bytes: DISK_WARN_AVAILABLE_BYTES,
                total_bytes: 4 * DISK_WARN_AVAILABLE_BYTES,
                detail: "ok".to_string(),
            },
            DiskHealthSnapshot {
                name: "workspace".to_string(),
                path: "/tmp/workspace".to_string(),
                status: DiskStatus::Warn,
                available_bytes: DISK_CRITICAL_AVAILABLE_BYTES,
                total_bytes: 4 * DISK_WARN_AVAILABLE_BYTES,
                detail: "tight".to_string(),
            },
        ];

        assert_eq!(
            disk_degraded_components(&disks),
            vec!["disk:workspace".to_string()]
        );
        assert_eq!(disk_degraded_label(&disks), "disk:workspace");
    }

    #[test]
    fn format_bytes_gib_is_operator_readable() {
        assert_eq!(format_bytes_gib(BYTES_PER_GIB), "1.0 GiB");
        assert_eq!(
            format_bytes_gib(BYTES_PER_GIB + (BYTES_PER_GIB / 2)),
            "1.5 GiB"
        );
    }
}
