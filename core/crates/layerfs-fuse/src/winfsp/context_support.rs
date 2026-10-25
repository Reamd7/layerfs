//! Free helpers of the WinFsp context: refusal mapping, DOS attribute faces
//! and the file info projection, split out of `context.rs` for its size.
use crate::{operations::Declined, ports::ServiceError};
use layerfs_content::filesystem::PathName;
use layerfs_content::object::inode_leaf::InodeKind;
use layerfs_overlay::OpenFile;
use layerfs_workspace::{Refusal, Time, ViewStat};
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_DISK_FULL, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_FILE_TOO_LARGE, STATUS_INTERNAL_ERROR, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_INVALID_PARAMETER, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_RETRY, STATUS_TOO_MANY_LINKS,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
};
use winfsp::{filesystem::FileInfo, FspError};

/// What one decided mutation published, copied out of the engine's custody.
pub(super) struct Mutated {
    pub stat: Option<ViewStat>,
    pub file: Option<OpenFile>,
}

pub(super) fn map_declined(declined: Declined) -> FspError {
    match declined {
        Declined::Refused(refusal) => map_refusal(refusal),
        Declined::NoSpace => FspError::from(STATUS_DISK_FULL),
        Declined::Contended => FspError::from(STATUS_RETRY),
        Declined::TargetTooLong => FspError::from(STATUS_OBJECT_NAME_INVALID),
    }
}

pub(super) fn map_refusal(refusal: Refusal) -> FspError {
    FspError::from(match refusal {
        Refusal::Exists => STATUS_OBJECT_NAME_COLLISION,
        Refusal::Missing => STATUS_OBJECT_NAME_NOT_FOUND,
        Refusal::NotDirectory => STATUS_NOT_A_DIRECTORY,
        Refusal::IsDirectory => STATUS_FILE_IS_A_DIRECTORY,
        Refusal::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        Refusal::NotPermitted
        | Refusal::Unsupported
        | Refusal::AncestryRequired
        | Refusal::AncestryMismatch => STATUS_ACCESS_DENIED,
        Refusal::Invalid => STATUS_INVALID_PARAMETER,
        Refusal::TooManyLinks => STATUS_TOO_MANY_LINKS,
        Refusal::TooLarge => STATUS_FILE_TOO_LARGE,
    })
}

pub(super) fn internal(error: &ServiceError) -> FspError {
    if error.is::<crate::ports::Fenced>() {
        FspError::from(STATUS_INVALID_DEVICE_REQUEST)
    } else {
        FspError::from(STATUS_INTERNAL_ERROR)
    }
}

/// One Windows FILETIME as the portable time; `None` for the zero that
/// means "leave it" or an unrepresentable value.
pub(super) fn unix_time(filetime: u64) -> Option<Time> {
    if filetime == 0 {
        return None;
    }
    let seconds = i64::try_from(filetime / 10_000_000)
        .ok()?
        .checked_sub(EPOCH_DELTA_SECONDS)?;
    let nanoseconds = u32::try_from((filetime % 10_000_000) * 100).ok()?;
    Some(Time {
        seconds,
        nanoseconds,
    })
}

/// A name Windows itself could never hold: an NTFS alternate-data-stream
/// colon, a reserved device stem (with any extension), or a trailing
/// dot/space.
pub(super) fn refused_name(name: &PathName) -> bool {
    let bytes = name.as_bytes();
    if bytes.contains(&b':') {
        return true;
    }
    if bytes
        .last()
        .is_some_and(|byte| *byte == b'.' || *byte == b' ')
    {
        return true;
    }
    let stem: Vec<u8> = bytes
        .split(|byte| *byte == b'.')
        .next()
        .unwrap_or(&[])
        .iter()
        .map(|byte| byte.to_ascii_uppercase())
        .collect();
    const RESERVED: [&[u8]; 22] = [
        b"CON", b"PRN", b"AUX", b"NUL", b"COM1", b"COM2", b"COM3", b"COM4", b"COM5", b"COM6",
        b"COM7", b"COM8", b"COM9", b"LPT1", b"LPT2", b"LPT3", b"LPT4", b"LPT5", b"LPT6", b"LPT7",
        b"LPT8", b"LPT9",
    ];
    RESERVED.contains(&stem.as_slice())
}

pub(super) fn attributes(stat: &ViewStat) -> u32 {
    match stat.kind {
        InodeKind::Directory => {
            // A write-disabled directory reports the read-only bit too.
            if stat.metadata.mode & 0o222 == 0 {
                FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_READONLY.0
            } else {
                FILE_ATTRIBUTE_DIRECTORY.0
            }
        }
        // This increment mounts read-only and reports no reparse points.
        // Every file carries the archive bit, as on NTFS; a write-disabled
        // mode adds the read-only bit. Hidden/system attributes have no
        // stored DOS face here (WINFSP-PORT.md).
        InodeKind::RegularFile | InodeKind::Symlink => {
            let mode = stat.metadata.mode;
            let archive = FILE_ATTRIBUTE_ARCHIVE;
            if mode & 0o222 == 0 {
                archive.0 | FILE_ATTRIBUTE_READONLY.0
            } else {
                archive.0
            }
        }
    }
}

/// Unix epoch to Windows FILETIME offset, in seconds.
pub(super) const EPOCH_DELTA_SECONDS: i64 = 11_644_473_600;

pub(super) fn filetime(seconds: i64, nanoseconds: u32) -> u64 {
    let shifted = seconds.checked_add(EPOCH_DELTA_SECONDS).unwrap_or(i64::MAX);
    u64::try_from(shifted)
        .unwrap_or(u64::MAX)
        .saturating_mul(10_000_000)
        .saturating_add(u64::from(nanoseconds) / 100)
}

pub(super) fn fill_info(file_info: &mut FileInfo, stat: &ViewStat) {
    file_info.file_attributes = attributes(stat);
    file_info.allocation_size = stat.logical_len;
    file_info.file_size = stat.logical_len;
    let time = filetime(stat.metadata.mtime_seconds, stat.metadata.mtime_nanoseconds);
    file_info.creation_time = time;
    file_info.last_access_time = time;
    file_info.last_write_time = time;
    file_info.change_time = time;
    file_info.index_number = stat.serial;
    file_info.hard_links = 0;
    file_info.ea_size = 0;
}
