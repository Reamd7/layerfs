//! `FileSystemContext` adapter: WinFsp callbacks over the portable
//! `operations` engine, one blocking service visit per callback.
use super::blocking::block_on;
use super::context_support::{
    attributes, filetime, fill_info, internal, map_declined, map_refusal, refused_name, unix_time,
    Mutated,
};
use crate::{
    dispatch::MountRequests,
    operations::{
        write as write_time, Declined, DirectoryStep, DirectoryStream, KernelData, KernelMutation,
        KernelRead, MutationInput, MutationRequest, ReadDataInput,
    },
    ports::{Fenced, MountServices, RequestServices, ServiceError},
};
use layerfs_content::{filesystem::PathName, object::inode_leaf::InodeKind};
use layerfs_overlay::{KernelMount, OpenFile, WRITE_WINDOW};
use layerfs_workspace::{KernelReadOperation, Refusal, ViewStat};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::SystemTime,
};
use windows::Win32::{
    Foundation::{
        STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_DISK_FULL,
        STATUS_FILE_IS_A_DIRECTORY, STATUS_FILE_TOO_LARGE, STATUS_INTERNAL_ERROR,
        STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER, STATUS_NOT_A_DIRECTORY,
        STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND,
        STATUS_RETRY, STATUS_TOO_MANY_LINKS,
    },
    Storage::FileSystem::{
        FILE_APPEND_DATA, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_READONLY, FILE_WRITE_DATA,
    },
};
use winfsp::{
    filesystem::{
        DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
        VolumeInfo, WideNameInfo,
    },
    FspError, U16CStr,
};

/// Mirrors the Linux STATFS declaration (`request/inline.rs`).
const DECLARED_BLOCKS: u64 = 1 << 30;
const BLOCK_BYTES: u64 = 4096;

/// Entries one open directory may remember before its attribute cache
/// resets, keeping enumeration state bounded.
const STATS_CACHE_LIMIT: usize = 10_000;

/// What one decided read owns, copied out of the engine's custody before the
/// reply: the inode's attributes and, for OPEN/OPENDIR, its descriptor.
struct Resolved {
    stat: ViewStat,
    file: Option<u64>,
    directory: Option<u64>,
}

/// One open WinFsp handle: the inode serial it names, the engine descriptor
/// `read_visit`/`close_file` expect, and this handle's enumeration state.
pub struct FileContext {
    serial: u64,
    handle: Option<u64>,
    directory: bool,
    dir_buffer: DirBuffer,
    /// Names already handed to WinFsp in this enumeration, with the reply
    /// cookie each one was offered at. WinFsp resumes enumeration by name;
    /// LayerFS resumes by cookie.
    markers: Mutex<HashMap<String, u64>>,
    /// Attributes already reported in this handle's enumeration, keyed by
    /// serial: a resumed read skips the GETATTR an entry already paid for.
    stats: Mutex<HashMap<u64, ViewStat>>,
    /// WinFsp's set_delete asks for deletion to happen at cleanup, not at
    /// the asking call; this carries the request between the two.
    pending_delete: Mutex<bool>,
}

impl FileContext {
    fn new(serial: u64, resolved: &Resolved) -> Self {
        let (handle, directory) = match (resolved.file, resolved.directory) {
            (Some(file), _) => (Some(file), false),
            (None, Some(directory)) => (Some(directory), true),
            (None, None) => (None, false),
        };
        Self {
            serial,
            handle,
            directory,
            dir_buffer: DirBuffer::new(),
            markers: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
            pending_delete: Mutex::new(false),
        }
    }
}

/// The WinFsp-facing file system: one mounted Workspace.
pub struct LayerFsContext {
    services: Arc<dyn MountServices>,
    queue: MountRequests,
    root: u64,
    requests: AtomicU64,
}

impl LayerFsContext {
    pub(super) fn new(queue: MountRequests, services: Arc<dyn MountServices>, root: u64) -> Self {
        Self {
            services,
            queue,
            root,
            requests: AtomicU64::new(1),
        }
    }

    fn mount(&self) -> KernelMount {
        self.queue.identity()
    }

    fn next_request(&self) -> u64 {
        self.requests.fetch_add(1, Ordering::Relaxed)
    }

    /// One request's services, refused only before any attempt.
    fn request_services(&self) -> Result<Arc<dyn RequestServices>, FspError> {
        self.services
            .request(&self.queue.fence())
            .map_err(|error| internal(&error))
    }

    /// Resolves a WinFsp path to the serial and attributes of its final
    /// name, one LOOKUP per component from the mount's committed root.
    fn resolve(&self, file_name: &U16CStr) -> Result<(u64, ViewStat), FspError> {
        let text = file_name.to_string_lossy();
        let components: Vec<&str> = text.split('\\').filter(|part| !part.is_empty()).collect();
        let services = self.request_services()?;
        let mount = self.mount();
        let mut serial = self.root;
        let mut stat = None;
        for component in &components {
            let name = PathName::from_bytes(component.as_bytes())
                .map_err(|_| FspError::from(STATUS_INVALID_PARAMETER))?;
            if refused_name(&name) {
                // Windows could never have held this name, so it cannot be
                // found here whatever the earlier components resolved to.
                return Err(FspError::from(STATUS_OBJECT_NAME_NOT_FOUND));
            }
            let operation = KernelReadOperation::Lookup {
                parent: serial,
                name,
            };
            let resolved = self
                .read_visit(&services, mount, serial, operation)?
                .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
            serial = resolved.stat.serial;
            stat = Some(resolved.stat);
        }
        match stat {
            Some(stat) => Ok((serial, stat)),
            // An empty path is the mount root itself.
            None => {
                let resolved = self
                    .read_visit(
                        &services,
                        mount,
                        serial,
                        KernelReadOperation::Getattr { serial },
                    )?
                    .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
                Ok((serial, resolved.stat))
            }
        }
    }

    /// Runs one read operation to a decided value; `None` is ENOENT. The
    /// reply's custody is disposed before the extracted values return.
    fn read_visit(
        &self,
        services: &Arc<dyn RequestServices>,
        mount: KernelMount,
        protected: u64,
        operation: KernelReadOperation,
    ) -> Result<Option<Resolved>, FspError> {
        let request = self.next_request();
        let read = block_on(KernelRead::prepare(
            services.clone(),
            mount,
            request,
            protected,
            None,
            operation,
        ))
        .map_err(|failure| internal(&failure.reason))?;
        let result = match read.value() {
            Ok(value) => Ok(Some(Resolved {
                stat: value.stat.clone(),
                file: value.file.as_ref().map(|file| file.owner_id()),
                directory: value
                    .directory
                    .as_ref()
                    .map(|directory| directory.owner_id()),
            })),
            Err(Refusal::Missing) => Ok(None),
            Err(refusal) => Err(map_refusal(refusal)),
        };
        let _ = block_on(read.dispose());
        result
    }
}

impl FileSystemContext for LayerFsContext {
    type FileContext = FileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [core::ffi::c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let (_, stat) = self.resolve(file_name)?;
        Ok(FileSecurity {
            reparse: false,
            // No ACL support: WinFsp falls back to its default descriptor.
            sz_security_descriptor: 0,
            attributes: attributes(&stat),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let (serial, stat) = self.resolve(file_name)?;
        let writable = granted_access & (FILE_WRITE_DATA.0 | FILE_APPEND_DATA.0) != 0;
        let operation = if stat.kind == InodeKind::Directory {
            KernelReadOperation::Opendir { serial }
        } else {
            KernelReadOperation::Open { serial, writable }
        };
        let services = self.request_services()?;
        let resolved = self
            .read_visit(&services, self.mount(), serial, operation)?
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        fill_info(file_info.as_mut(), &resolved.stat);
        let context = FileContext::new(serial, &resolved);
        // `DeleteFileW`/`RemoveDirectoryW` open the existing name with the
        // delete declared at open (FILE_DELETE_ON_CLOSE), never a SetDelete
        // call: carry it to Cleanup the same way. A non-empty directory
        // opened this way still opens — the delete happens at Cleanup, and
        // its refusal there is silent, as the delete-on-close contract is.
        const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;
        if create_options & FILE_DELETE_ON_CLOSE != 0 {
            *context
                .pending_delete
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = true;
        }
        Ok(context)
    }

    fn close(&self, context: Self::FileContext) {
        let Some(handle) = context.handle else {
            return;
        };
        let Ok(services) = self.request_services() else {
            // A stopped fence ends the mount; nothing is released on it.
            return;
        };
        let mount = self.mount();
        let request = self.next_request();
        let future = if context.directory {
            services.close_directory(mount, request, handle)
        } else {
            services.close_file(mount, context.serial, handle)
        };
        let _ = block_on(future);
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let services = self.request_services()?;
        let resolved = self
            .read_visit(
                &services,
                self.mount(),
                context.serial,
                KernelReadOperation::Getattr {
                    serial: context.serial,
                },
            )?
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        fill_info(file_info, &resolved.stat);
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        // The engine caps one read visit at READ_WINDOW; the kernel asks for
        // whatever its caches batched, so visits chain until the buffer is
        // full or the file ends short.
        const WINDOW: usize = layerfs_overlay::READ_WINDOW;
        let services = self.request_services()?;
        let mut filled = 0usize;
        let mut position = offset;
        while filled < buffer.len() {
            let room = buffer.len() - filled;
            let span = room.min(WINDOW);
            let length = u32::try_from(span).unwrap_or(u32::MAX);
            let request = self.next_request();
            let answer = block_on(KernelData::read(
                services.clone(),
                self.mount(),
                request,
                context.serial,
                context.handle,
                ReadDataInput::File {
                    offset: position,
                    length,
                },
            ))
            .map_err(|failure| internal(&failure.reason))?;
            let data = answer.map_err(map_refusal)?;
            let bytes = data.bytes();
            let copied = bytes.len().min(span);
            buffer[filled..filled + copied].copy_from_slice(&bytes[..copied]);
            filled += copied;
            position += copied as u64;
            // A short answer is the file's end; nothing further to fill.
            if copied < span {
                break;
            }
        }
        Ok(filled as u32)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let handle = context
            .handle
            .ok_or_else(|| FspError::from(STATUS_INVALID_PARAMETER))?;
        // WinFsp's reader is stateless across calls: a marker names a
        // position, and the callback must be able to serve it whatever this
        // handle saw before. A fresh start, or a marker this context never
        // offered (the driver may resume on a freshly opened context),
        // rebuilds the whole listing; a known marker reads the buffer that
        // rebuild already filled.
        let known = match marker.inner() {
            None => false,
            Some(name) => {
                let key = String::from_utf16_lossy(name);
                context
                    .markers
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .contains_key(&key)
            }
        };
        if !known {
            // A rebuilt listing reports current facts; cached ones may
            // predate mutations from other handles.
            context
                .stats
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clear();
            self.rebuild_listing(context, handle)?;
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let total = DECLARED_BLOCKS * BLOCK_BYTES;
        out_volume_info.total_size = total;
        out_volume_info.free_size = total;
        out_volume_info.set_volume_label("layerfs");
        Ok(())
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_attributes: u32,
        _security_descriptor: Option<&[core::ffi::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let (_, leaf) = self.path_components(file_name)?;
        if refused_name(&leaf) {
            // A name Windows itself could never hold cannot be created.
            return Err(FspError::from(STATUS_OBJECT_NAME_INVALID));
        }
        let (parent, name) = self.resolve_parent(file_name)?;
        let writable = granted_access & (FILE_WRITE_DATA.0 | FILE_APPEND_DATA.0) != 0;
        // A create's read-only attribute maps to the write-disabled mode;
        // other DOS attributes have no stored face here (WINFSP-PORT.md).
        let readonly = file_attributes & FILE_ATTRIBUTE_READONLY.0 != 0;
        let input = if create_options & 0x0000_0001 != 0 {
            MutationInput::Named(layerfs_workspace::Operation::Mkdir {
                parent,
                name,
                mode: if readonly { 0o555 } else { 0o755 },
            })
        } else {
            MutationInput::Named(layerfs_workspace::Operation::Create {
                parent,
                name,
                mode: if readonly { 0o444 } else { 0o644 },
            })
        };
        let mutated = self.mutate(parent, None, input, Some(writable))?;
        let stat = mutated
            .stat
            .clone()
            .ok_or_else(|| FspError::from(STATUS_INTERNAL_ERROR))?;
        let serial = stat.serial;
        let resolved = Resolved {
            stat,
            file: mutated.file.map(|file| file.owner_id()),
            directory: None,
        };
        fill_info(file_info.as_mut(), &resolved.stat);
        let context = FileContext::new(serial, &resolved);
        // `DeleteFileW`/`RemoveDirectoryW` declare the delete at Create
        // (FILE_DELETE_ON_CLOSE) instead of a SetDelete call: carry it to
        // Cleanup exactly as set_delete would.
        const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;
        if create_options & FILE_DELETE_ON_CLOSE != 0 {
            *context
                .pending_delete
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = true;
        }
        Ok(context)
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // The DOS attribute face this port stores: the read-only bit is the
        // write-disabled mode (a clear adds the owner's write bit back);
        // hidden/system hold no stored face. A written time is the mtime.
        let services = self.request_services()?;
        let current = self
            .read_visit(
                &services,
                self.mount(),
                context.serial,
                KernelReadOperation::Getattr {
                    serial: context.serial,
                },
            )?
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let mode = u32::from(current.stat.metadata.mode);
        let mode = if file_attributes & FILE_ATTRIBUTE_READONLY.0 != 0 {
            mode & !0o222
        } else {
            mode | 0o200
        };
        let mtime = unix_time(last_write_time);
        let input = MutationInput::Named(layerfs_workspace::Operation::SetAttributes {
            serial: context.serial,
            mode: Some(mode),
            mtime,
            size: None,
        });
        let mutated = self.mutate(context.serial, None, input, None)?;
        if let Some(stat) = &mutated.stat {
            fill_info(file_info, stat);
        }
        Ok(())
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        // A paging write out of a mapping is constrained: it may not extend
        // the file, and what would land past the current end is dropped.
        // A write-to-end positions at the current end instead.
        let services = if write_to_eof || constrained_io {
            Some(self.request_services()?)
        } else {
            None
        };
        let current = match &services {
            Some(services) => Some(
                self.read_visit(
                    services,
                    self.mount(),
                    context.serial,
                    KernelReadOperation::Getattr {
                        serial: context.serial,
                    },
                )?
                .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?
                .stat
                .logical_len,
            ),
            None => None,
        };
        let mut position = match (write_to_eof, current) {
            (true, Some(end)) => end,
            _ => offset,
        };
        let limit = match (constrained_io, current) {
            (true, Some(end)) => Some(end),
            _ => None,
        };
        // One mutation per WRITE_WINDOW, exactly as the FUSE path batches.
        let mut written = 0u32;
        let mut last: Option<ViewStat> = None;
        'windows: for window in buffer.chunks(WRITE_WINDOW) {
            if let Some(limit) = limit {
                if position >= limit {
                    break 'windows;
                }
                if position + window.len() as u64 > limit {
                    let room = (limit - position) as usize;
                    let window = &window[..room];
                    let input = MutationInput::Write {
                        offset: position,
                        data: window.to_vec().into(),
                        cached: false,
                    };
                    let mutated = self.mutate(context.serial, context.handle, input, None)?;
                    position += room as u64;
                    written += room as u32;
                    last = mutated.stat.or(last);
                    break 'windows;
                }
            }
            let input = MutationInput::Write {
                offset: position,
                data: window.to_vec().into(),
                cached: false,
            };
            let mutated = self.mutate(context.serial, context.handle, input, None)?;
            position += window.len() as u64;
            written += window.len() as u32;
            last = mutated.stat.or(last);
        }
        if let Some(stat) = &last {
            fill_info(file_info, stat);
        }
        Ok(written)
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let input = MutationInput::Attributes {
            mode: None,
            mtime: None,
            size: Some(0),
        };
        let mutated = self.mutate(context.serial, context.handle, input, None)?;
        if let Some(stat) = &mutated.stat {
            fill_info(file_info, stat);
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if set_allocation_size {
            // Allocation size is a hint; LayerFS has no preallocation.
            file_info.allocation_size = new_size;
            return Ok(());
        }
        let input = MutationInput::Attributes {
            mode: None,
            mtime: None,
            size: Some(new_size),
        };
        let mutated = self.mutate(context.serial, context.handle, input, None)?;
        if let Some(stat) = &mutated.stat {
            fill_info(file_info, stat);
        }
        Ok(())
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let (parent, name) = self.resolve_parent(file_name)?;
        let (_, leaf) = self.path_components(new_file_name)?;
        if refused_name(&leaf) {
            // Renaming onto a name Windows itself could never hold is as
            // invalid as creating it.
            return Err(FspError::from(STATUS_OBJECT_NAME_INVALID));
        }
        let (new_parent, new_name) = self.resolve_parent(new_file_name)?;
        if replace_if_exists {
            // Windows never replaces a directory by rename, whatever the
            // engine's empty-directory grammar would allow.
            if let Some(services) = self.request_services().ok() {
                if let Some(resolved) = self
                    .read_visit(
                        &services,
                        self.mount(),
                        new_parent,
                        KernelReadOperation::Lookup {
                            parent: new_parent,
                            name: new_name.clone(),
                        },
                    )?
                    .filter(|resolved| resolved.stat.kind == InodeKind::Directory)
                {
                    let _ = resolved;
                    return Err(FspError::from(STATUS_ACCESS_DENIED));
                }
            }
        }
        // Root-relative ancestry evidence of the destination parent, which a
        // directory rename across parents requires.
        let destination_path = self.root_path(new_file_name);
        let input = MutationInput::Named(layerfs_workspace::Operation::Rename {
            parent,
            name,
            new_parent,
            new_name,
            replace: replace_if_exists,
            destination_path,
        });
        self.mutate(parent, None, input, None)?;
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if delete_file && context.directory {
            // The disposition the caller wants to set: a directory must be
            // deletable now, or the set fails exactly as `RemoveDirectoryW`
            // fails on a non-empty one.
            let services = self.request_services()?;
            let handle = context
                .handle
                .ok_or_else(|| FspError::from(STATUS_INTERNAL_ERROR))?;
            if !self.directory_empty(&services, context.serial, handle)? {
                return Err(FspError::from(STATUS_DIRECTORY_NOT_EMPTY));
            }
        }
        *context
            .pending_delete
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = delete_file;
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, _flags: u32) {
        let pending = *context
            .pending_delete
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !pending {
            return;
        }
        let Some(name) = file_name else {
            return;
        };
        let Ok((parent, name)) = self.resolve_parent(name) else {
            return;
        };
        let input = if context.directory {
            MutationInput::Named(layerfs_workspace::Operation::Rmdir { parent, name })
        } else {
            MutationInput::Named(layerfs_workspace::Operation::Unlink { parent, name })
        };
        let _ = self.mutate(parent, None, input, None);
    }

    fn flush(
        &self,
        _context: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // Durability is the Workspace commit's, not the kernel's.
        Ok(())
    }
}

impl LayerFsContext {
    /// Whether one directory holds any entry beyond its dots; one stream
    /// visit, disposed before the answer returns.
    fn directory_empty(
        &self,
        services: &Arc<dyn RequestServices>,
        serial: u64,
        handle: u64,
    ) -> Result<bool, FspError> {
        let mut stream = block_on(DirectoryStream::prepare(
            services.clone(),
            self.mount(),
            self.next_request(),
            serial,
            handle,
            0,
        ))
        .map_err(|failure| internal(&failure.reason))?;
        loop {
            match block_on(stream.next()).map_err(|failure| internal(&failure.reason))? {
                DirectoryStep::End(_) => return Ok(true),
                DirectoryStep::Batch(batch) => {
                    if batch.entries().next().is_some() {
                        let _ = block_on(batch.accept(0));
                        return Ok(false);
                    }
                    stream =
                        block_on(batch.accept(0)).map_err(|failure| internal(&failure.reason))?;
                }
            }
        }
    }
    /// The serial of one name inside the enumerated directory; the stream's
    /// batch entries already carried it, but the portable batch type keeps
    /// only names and cookies, so one LOOKUP recovers it.
    fn serial_of(
        &self,
        services: &Arc<dyn RequestServices>,
        name: &[u8],
        parent: u64,
        handle: u64,
    ) -> Result<u64, FspError> {
        let _ = handle;
        let name =
            PathName::from_bytes(name).map_err(|_| FspError::from(STATUS_INVALID_PARAMETER))?;
        let resolved = self
            .read_visit(
                services,
                self.mount(),
                parent,
                KernelReadOperation::Lookup { parent, name },
            )?
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        Ok(resolved.stat.serial)
    }

    fn stat_of(
        &self,
        services: &Arc<dyn RequestServices>,
        serial: u64,
    ) -> Result<ViewStat, FspError> {
        let resolved = self
            .read_visit(
                services,
                self.mount(),
                serial,
                KernelReadOperation::Getattr { serial },
            )?
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        Ok(resolved.stat)
    }
}

impl LayerFsContext {
    /// Chains the engine's bounded windows from the start into the handle's
    /// directory buffer, until an empty window proves the directory's end.
    fn rebuild_listing(&self, context: &FileContext, handle: u64) -> winfsp::Result<()> {
        let services = self.request_services()?;
        let lock = context.dir_buffer.acquire(true, None)?;
        let mut names: Vec<(String, u64)> = Vec::new();
        let mut resume = 0u64;
        loop {
            let mut stream = block_on(DirectoryStream::prepare(
                services.clone(),
                self.mount(),
                self.next_request(),
                context.serial,
                handle,
                resume,
            ))
            .map_err(|failure| internal(&failure.reason))?;
            for (name, serial, cookie) in stream.dots() {
                let mut entry: DirInfo = DirInfo::new();
                entry.file_info_mut().file_attributes = FILE_ATTRIBUTE_DIRECTORY.0;
                entry.file_info_mut().index_number = serial;
                entry.set_name(name)?;
                lock.write(&mut entry)?;
                names.push((name.to_owned(), cookie));
            }
            let mut last_cookie = None;
            loop {
                match block_on(stream.next()).map_err(|failure| internal(&failure.reason))? {
                    DirectoryStep::End(_) => break,
                    DirectoryStep::Batch(batch) => {
                        let entries: Vec<(Vec<u8>, u64)> = batch
                            .entries()
                            .map(|(entry, cookie)| (entry.name.clone(), cookie))
                            .collect();
                        let mut written = 0usize;
                        for (name, cookie) in &entries {
                            let serial = self.serial_of(&services, name, context.serial, handle)?;
                            let cached = context
                                .stats
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .get(&serial)
                                .cloned();
                            let stat = match cached {
                                Some(stat) => stat,
                                None => {
                                    let stat = self.stat_of(&services, serial)?;
                                    let mut stats = context
                                        .stats
                                        .lock()
                                        .unwrap_or_else(|poison| poison.into_inner());
                                    if stats.len() > STATS_CACHE_LIMIT {
                                        stats.clear();
                                    }
                                    stats.insert(serial, stat.clone());
                                    stat
                                }
                            };
                            let mut entry: DirInfo = DirInfo::new();
                            entry.file_info_mut().file_attributes = attributes(&stat);
                            entry.file_info_mut().index_number = serial;
                            let text = String::from_utf8_lossy(name).into_owned();
                            entry.set_name(&text)?;
                            lock.write(&mut entry)?;
                            names.push((text, *cookie));
                            last_cookie = Some(*cookie);
                            written += 1;
                        }
                        stream = block_on(batch.accept(written))
                            .map_err(|failure| internal(&failure.reason))?;
                    }
                }
            }
            // The next window anchors at this window's last entry: the
            // engine's cursor rule refuses one past a published name, and
            // the window it hands out starts after the anchor's own name.
            match last_cookie {
                Some(next) if next > resume => resume = next,
                _ => break,
            }
        }
        drop(lock);
        let mut markers = context
            .markers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for (name, cookie) in names {
            markers.insert(name, cookie);
        }
        Ok(())
    }
    /// Resolves a path to its parent serial and final name.
    fn resolve_parent(&self, file_name: &U16CStr) -> Result<(u64, PathName), FspError> {
        let (parent, name) = self.path_components(file_name)?;
        if refused_name(&name) {
            // The name cannot exist, so there is nothing to resolve to.
            return Err(FspError::from(STATUS_OBJECT_NAME_NOT_FOUND));
        }
        let services = self.request_services()?;
        let mut serial = self.root;
        for component in &parent {
            let resolved = self
                .read_visit(
                    &services,
                    self.mount(),
                    serial,
                    KernelReadOperation::Lookup {
                        parent: serial,
                        name: component.clone(),
                    },
                )?
                .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
            serial = resolved.stat.serial;
        }
        Ok((serial, name))
    }

    /// Splits a WinFsp path into parent components and the final name, both
    /// owned. A root path has no name to operate on and is refused.
    fn path_components(&self, file_name: &U16CStr) -> Result<(Vec<PathName>, PathName), FspError> {
        let text = file_name.to_string_lossy();
        let mut parts = text
            .split('\\')
            .filter(|part| !part.is_empty())
            .map(|part| {
                PathName::from_bytes(part.as_bytes())
                    .map_err(|_| FspError::from(STATUS_INVALID_PARAMETER))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let name = parts
            .pop()
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_INVALID))?;
        Ok((parts, name))
    }

    /// The root-relative path of every component before the final name.
    fn root_path(&self, file_name: &U16CStr) -> Option<Vec<PathName>> {
        self.path_components(file_name)
            .ok()
            .map(|(parent, _)| parent)
    }

    /// Runs one mutation to its deciding job, releases its publication
    /// ticket, and returns the published facts copied out of custody.
    fn mutate(
        &self,
        protected: u64,
        handle: Option<u64>,
        input: MutationInput,
        open: Option<bool>,
    ) -> Result<Mutated, FspError> {
        let services = self.request_services()?;
        let Some(now) = write_time::time(SystemTime::now()) else {
            return Err(FspError::from(STATUS_INTERNAL_ERROR));
        };
        let request = MutationRequest {
            mount: self.mount(),
            request: self.next_request(),
            protected,
            handle,
            input,
            open,
            now,
        };
        let mutation = block_on(KernelMutation::perform(services, request))
            .map_err(|failure| internal(&failure.reason))?;
        let value = match mutation.value() {
            Ok(published) => Mutated {
                stat: published.stat.clone(),
                file: published.file,
            },
            Err(declined) => return Err(map_declined(declined)),
        };
        let _ = block_on(mutation.replied());
        Ok(value)
    }
}
