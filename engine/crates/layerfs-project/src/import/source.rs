//! Host source observations; symbolic links are stored without traversal.
use crate::{ProjectError, SourceRefusal};
use layerfs_content::filesystem::symlink::SymlinkTarget;
use std::{
    fs::{self, Metadata},
    path::Path,
};

pub(super) fn read_link(path: &Path, scanned: &Metadata) -> Result<Vec<u8>, ProjectError> {
    // read_link never follows the target, including dangling, external or cyclic
    // links. A replaced/changed source is refused without a second read/retry.
    let target = fs::read_link(path)?;
    let after = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !after.file_type().is_symlink()
            || after.dev() != scanned.dev()
            || after.ino() != scanned.ino()
            || after.len() != scanned.len()
            || after.mode() != scanned.mode()
            || after.mtime() != scanned.mtime()
            || after.mtime_nsec() != scanned.mtime_nsec()
            || after.ctime() != scanned.ctime()
            || after.ctime_nsec() != scanned.ctime_nsec()
        {
            return Err(ProjectError::refused(SourceRefusal::Changed, path));
        }
    }
    // Windows has no dev/ino in std: the change evidence is the whole stamp.
    #[cfg(windows)]
    {
        use super::backing::Stamp;
        if !after.file_type().is_symlink() || Stamp::of(&after) != Stamp::of(scanned) {
            return Err(ProjectError::refused(SourceRefusal::Changed, path));
        }
    }
    // Keep the canonical target bound/grammar: no normalization or UTF-8 cast.
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        target.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes = target.to_string_lossy().into_owned().into_bytes();
    let target = SymlinkTarget::new(bytes).map_err(ProjectError::Content)?;
    Ok(target.as_bytes().to_vec())
}

/// Refuses a backing whose files are the source or lie inside it.
///
/// The placement is resolved through its links first, then each ancestor is
/// compared with the source's identity, so a symlinked or otherwise aliased
/// spelling of a directory inside the source is recognised. Nothing is
/// begun or changed: a backing inside the source would both alter the source
/// while it is read and be acquired as part of it.
pub(super) fn outside_source(
    source_path: &Path,
    source: &Metadata,
    placement: &Path,
) -> Result<(), ProjectError> {
    let mut ancestor = fs::canonicalize(placement)?;
    loop {
        let metadata = fs::symlink_metadata(&ancestor)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.dev() == source.dev() && metadata.ino() == source.ino() {
                return Err(ProjectError::BackingInsideSource);
            }
        }
        // Windows: one canonicalized spelling of a directory, compared
        // case-insensitively. Directories have no hard links here and
        // canonicalization resolves every junction and symlink, so an equal
        // spelling is an alias and anything else is not. The previous weak
        // stamp comparison refused fresh sibling directories created in the
        // same instant as the source.
        #[cfg(windows)]
        {
            let _ = &metadata;
            if path_equals(&ancestor, source_path) {
                return Err(ProjectError::BackingInsideSource);
            }
        }
        if !ancestor.pop() {
            return Ok(());
        }
    }
}

/// Case-insensitive path equality, Windows's namespace rule.
#[cfg(windows)]
fn path_equals(one: &Path, other: &Path) -> bool {
    if one.as_os_str().eq_ignore_ascii_case(other.as_os_str()) {
        return true;
    }
    // Spellings that differ before canonicalization still alias when they
    // canonicalize to one directory.
    match (fs::canonicalize(one), fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one.as_os_str().eq_ignore_ascii_case(other.as_os_str()),
        _ => false,
    }
}
