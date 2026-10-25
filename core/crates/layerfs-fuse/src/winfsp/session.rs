//! WinFsp session lifecycle: attach one Workspace to a fresh directory,
//! detach by stopping the host. No mount-table receipts; teardown is the
/// host's own unmount.
use super::context::LayerFsContext;
use crate::{dispatch::MountRequests, ports::MountServices};
use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use winfsp::{
    host::{FileSystemHost, FineGuard, VolumeParams},
    winfsp_init_or_die, FspInit,
};

/// Configuration for one attach attempt. `root` is the committed root serial
/// the mount projects (the Linux `Identity::root`).
#[derive(Clone, Debug)]
pub struct WinFspSessionConfig {
    /// A new directory created by this attach; an existing path is refused.
    pub directory: PathBuf,
    /// The Workspace's committed root serial.
    pub root: u64,
}

/// One mounted WinFsp file system. Dropping it performs the same teardown as
/// [`WinFspSession::detach`].
pub struct WinFspSession {
    host: Option<FileSystemHost<LayerFsContext, FineGuard>>,
    directory: PathBuf,
    /// Keeps the WinFsp library initialized for the host's lifetime.
    _init: Option<FspInit>,
}

impl fmt::Debug for WinFspSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WinFspSession")
            .field("directory", &self.directory)
            .field("mounted", &self.host.is_some())
            .finish_non_exhaustive()
    }
}

impl WinFspSession {
    /// Starts the WinFsp host at the mount point. The point itself must be
    /// absent (WinFsp creates it with the volume); a stale empty directory is
    /// removed, and the parent is created when missing.
    pub fn attach(
        config: WinFspSessionConfig,
        queue: MountRequests,
        services: Arc<dyn MountServices>,
    ) -> io::Result<Self> {
        let init = winfsp_init_or_die();
        if let Some(parent) = config.directory.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::symlink_metadata(&config.directory) {
            Ok(_) => {
                if fs::remove_dir(&config.directory).is_err() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "the mount point exists and is not an empty directory",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let started = (|| -> io::Result<FileSystemHost<LayerFsContext, FineGuard>> {
            let context = LayerFsContext::new(queue, services, config.root);
            let mut params = VolumeParams::new();
            params
                .sector_size(4096)
                .sectors_per_allocation_unit(1)
                .volume_creation_time(0)
                .file_info_timeout(1000)
                .case_sensitive_search(true)
                .case_preserved_names(true)
                .unicode_on_disk(true)
                .persistent_acls(false)
                .filesystem_name("layerfs");
            let mut host: FileSystemHost<LayerFsContext, FineGuard> =
                FileSystemHost::new(params, context)
                    .map_err(|error| io::Error::other(error.to_string()))?;
            host.mount(mount_point(&config.directory)?)?;
            host.start()
                .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(host)
        })();
        match started {
            Ok(host) => Ok(Self {
                host: Some(host),
                directory: config.directory,
                _init: Some(init),
            }),
            Err(error) => {
                let _ = fs::remove_dir(&config.directory);
                Err(error)
            }
        }
    }

    /// Whether the host is still mounted.
    pub fn mounted(&self) -> bool {
        self.host.is_some()
    }

    /// Stops the host, unmounts and removes the mount directory. Idempotent.
    pub fn detach(&mut self) {
        if let Some(mut host) = self.host.take() {
            host.stop();
            host.unmount();
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

impl Drop for WinFspSession {
    fn drop(&mut self) {
        self.detach();
    }
}

fn mount_point(directory: &Path) -> io::Result<String> {
    directory
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mount path must be UTF-8"))
}
