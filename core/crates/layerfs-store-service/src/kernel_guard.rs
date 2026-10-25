//! Whether this process may open the global Store directly (#352 F-020).
//!
//! Every opener of one Store must share one kernel and the local
//! filesystem that holds it: SQLite's WAL locks and the writers' admission
//! lane (a `flock` on the Store's directory) coordinate only within one
//! kernel. On Linux the guard verifies the filesystem holding the Store's
//! directory against an allowlist of local filesystems.
//!
//! Windows is native single-kernel access (owner direction 2026-10-25: no
//! WSL worker; direct Store access must run on Windows itself). All
//! openers share the one Windows kernel, SQLite's own byte-range locking
//! on NTFS coordinates them and the writers' admission relies on those
//! SQLite locks, exactly as on Darwin. The guard only checks that the
//! Store's directory can be examined before anything is opened.
//! Anything unverifiable is `Unsupported` with its reason; there is no
//! other route to fall back to.
use std::path::Path;

/// Filesystems that carry the Store's locks: ext4, xfs and btrfs.
#[cfg(target_os = "linux")]
const LOCAL: [(nix::sys::statfs::FsType, &str); 3] = [
    (nix::sys::statfs::EXT4_SUPER_MAGIC, "ext4"),
    (nix::sys::statfs::XFS_SUPER_MAGIC, "xfs"),
    (nix::sys::statfs::BTRFS_SUPER_MAGIC, "btrfs"),
];

/// Why direct Store access is refused here: the reason of `Unsupported`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Checks the platform and the filesystem holding `store`'s directory,
/// before anything is opened. Returns the filesystem's name.
pub fn check(store: &Path) -> Result<&'static str, Unsupported> {
    #[cfg(target_os = "linux")]
    {
        let directory = store
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let stat = nix::sys::statfs::statfs(directory).map_err(|error| {
            Unsupported(format!(
                "the Store's directory {} cannot be examined: {error}",
                directory.display()
            ))
        })?;
        let kind = stat.filesystem_type();
        LOCAL
            .iter()
            .find(|(magic, _)| *magic == kind)
            .map(|(_, name)| *name)
            .ok_or_else(|| {
                Unsupported(format!(
                    "filesystem 0x{:x} at {} does not carry SQLite locks between kernels; \
                     direct Store access needs ext4, xfs or btrfs",
                    kind.0,
                    directory.display()
                ))
            })
    }
    #[cfg(target_os = "windows")]
    {
        let directory = store
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::symlink_metadata(directory).map_err(|error| {
            Unsupported(format!(
                "the Store's directory {} cannot be examined: {error}",
                directory.display()
            ))
        })?;
        Ok("ntfs")
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = store;
        Err(Unsupported(
            "direct Store access needs the daemons' Linux kernel".to_owned(),
        ))
    }
}
