//! Experimental WinFsp mount backend for Windows.
//!
//! This module is the Windows counterpart of the Linux `mount`/`request`/
//! `session` modules: it attaches one Workspace as a WinFsp file system on a
//! fresh local directory, without Docker or WSL. It reuses the portable
//! `operations` protocol engine and the `dispatch` queue unchanged; only the
//! kernel bridge is different (WinFsp user-mode callbacks instead of
//! `/dev/fuse`).
//!
//! Status of this backend (first increment):
//!
//! - Mounted **read-only**. Every mutating callback keeps its trait default
//!   (`STATUS_INVALID_DEVICE_REQUEST`): `create`, `write`, `rename`,
//!   `set_basic_info`, `set_delete`, `set_file_size`, `overwrite`. The
//!   mutation path (`operations::mutation`) is not wired yet.
//! - Symlinks are reported as plain files; reparse points are not mapped.
//! - Security descriptors are not synthesized: WinFsp falls back to its
//!   default descriptor, and only file attributes are reported.
//! - Names cross the WinFsp UTF-16 boundary as UTF-8; Store names that are
//!   not valid UTF-8 are served lossily, matching the Windows namespace.
//! - The Linux session's per-step receipts (mount table row, abort control
//!   write, negotiation facts) have no WinFsp equivalent and are absent.
mod blocking;
mod context;
mod session;

pub use context::{FileContext, LayerFsContext};
pub use session::{WinFspSession, WinFspSessionConfig};

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_context_implements_winfsp()
    where
        LayerFsContext: winfsp::filesystem::FileSystemContext<FileContext = FileContext>,
    {
    }

    /// The backend's public surface: config shape and trait coherence. A
    /// real attach needs the WinFsp driver and an engine-minted mount.
    #[test]
    fn winfsp_backend_surface() {
        let config = WinFspSessionConfig {
            directory: "C:\\layerfs-no-such-mount".into(),
            root: 1,
        };
        assert_eq!(config.root, 1);
        assert_context_implements_winfsp();
    }
}
