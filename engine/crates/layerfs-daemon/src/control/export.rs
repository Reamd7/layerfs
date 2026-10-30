//! WinFsp kernel serving: the Windows native route's mount entry, beside FUSE.
//!
//! Each Attach serves the Workspace's engine mount as one WinFsp file system
//! on a fresh mount directory (`layerfs_fuse::winfsp`). The registry keeps
//! the session as the entry's kernel half; the control records and every
//! engine and Store step are the ones the FUSE route uses:
//!
//! - Ready carries the same receipt record; a WinFsp receipt is recognized
//!   by `abi_major == 0`, and its `mount_id` is the engine mount's owner id.
//! - Commit needs no flush or refresh step: this port serves WinFsp's
//!   default write-through (no client write-back cache) and owns the
//!   attribute timeout itself.
//! - Unmount stops the host — WinFsp waits out open handles, so the stop is
//!   bounded here on a thread of its own — then revokes the engine mount
//!   and runs the one logical Close. A step that is not established keeps
//!   the session and its engine mount in the entry, as FUSE custody is kept.
//! - ForceUnmount runs the same teardown; WinFsp has no busy probe, so it
//!   differs from a normal unmount only in the guard and the receipt.
use super::{
    kernel::{bounded, Kernel, KernelConfig},
    registry::{bound_mut, idle},
    Failure, Service, Success,
};
use crate::store::BoundWorkspace;
use layerfs_bridge::control::{
    AbortDisposition, Activity, ControlCode, DetachDisposition, ForceUnmounted, ForcedCleanup,
    ForcedFacts, ForcedOutcome, MountPhase, MountReceipt, MountWork, ReadyMount, Reply,
    TeardownCustody, TeardownStage, WorkspaceToken, MOUNT_DIRECTORY_LIMIT,
};
use layerfs_fuse::{
    winfsp::{WinFspSession, WinFspSessionConfig},
    MountRequests,
};
use layerfs_overlay::{CleanupState, KernelMount};
use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// How long one host stop may wait out open handles before the entry keeps
/// the session as custody instead.
const DETACH_WAIT: Duration = Duration::from_secs(60);
/// Polling step of the bounded host stop.
const DETACH_POLL: Duration = Duration::from_millis(20);

/// Process-wide WinFsp serving: configuration only; each Attach starts its
/// own host and each terminal operation stops it.
#[derive(Debug)]
pub struct WinFspServing {
    pub(super) config: KernelConfig,
}
impl WinFspServing {
    /// Starts serving once: the mount home is created (or checked, when it
    /// exists) before control readiness. No file system is mounted until an
    /// explicit Attach.
    pub fn start(config: KernelConfig) -> std::io::Result<Self> {
        let home = &config.mounts;
        if !home.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mount home must be absolute",
            ));
        }
        std::fs::create_dir_all(home)?;
        if !std::fs::symlink_metadata(home)?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mount home is not a directory",
            ));
        }
        Ok(Self { config })
    }
    pub fn config(&self) -> &KernelConfig {
        &self.config
    }
    /// A namespace is never reused by this daemon, so neither is its
    /// directory; the short hex of the namespace is its name.
    pub(super) fn directory(&self, token: WorkspaceToken) -> PathBuf {
        let namespace = u64::try_from(token.namespace).unwrap_or(u64::MAX);
        self.config.mounts.join(format!("w{namespace:016x}"))
    }
    pub(super) fn register(&self, mount: KernelMount) -> MountRequests {
        MountRequests::new(mount)
    }
}
/// A serving WinFsp session and the owners the registry entry keeps.
pub(super) struct WinFspAttached {
    pub session: WinFspSession,
    pub ready: ReadyMount,
    pub mount: KernelMount,
    pub queue: MountRequests,
}
/// A terminal operation owns the session; status still observes it.
pub(super) struct WinFspLeaving {
    pub ready: ReadyMount,
    pub phase: MountPhase,
}
/// WinFsp custody a terminal or attach step could not finish disposing: kept
/// exactly, never retried or settled by a later observation.
pub(super) struct WinFspKept {
    pub ready: Option<ReadyMount>,
    pub stage: TeardownStage,
    pub detail: String,
    pub evidence: Box<dyn fmt::Debug + Send>,
    /// Effects of a forced teardown as recorded where it stopped; absent for
    /// a normal unmount or an Attach.
    pub forced: Option<ForcedFacts>,
}
impl fmt::Debug for WinFspKept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WinFspKept")
            .field("ready", &self.ready)
            .field("stage", &self.stage)
            .field("detail", &self.detail)
            .field("evidence", &self.evidence)
            .field("forced", &self.forced)
            .finish()
    }
}
impl WinFspKept {
    /// WinFsp custody is never forceable: its host stop has no separate
    /// abort control whose one write could still be pending.
    pub(super) fn forceable(&self) -> bool {
        false
    }
    pub(super) fn custody(&self, token: WorkspaceToken) -> TeardownCustody {
        TeardownCustody {
            token,
            stage: self.stage,
            detached: matches!(self.stage, TeardownStage::Revoke | TeardownStage::Close),
            work: None,
            detail: bounded(&self.detail),
            forced: self.forced.clone(),
        }
    }
}
fn kernel_failure(code: ControlCode, phase: &'static str, detail: impl Into<String>) -> Failure {
    Failure::Kernel(Box::new(super::types::KernelFailure {
        code,
        phase,
        detail: detail.into(),
        evidence: None,
    }))
}
fn receipt(token: WorkspaceToken, directory: String, mount: &KernelMount, root: u64) -> ReadyMount {
    ReadyMount {
        token,
        directory,
        receipt: MountReceipt {
            mount: mount.owner_id(),
            root,
            mount_id: mount.owner_id(),
            device_major: 0,
            device_minor: 0,
            // No FUSE ABI: a WinFsp receipt is recognized by abi_major == 0.
            abi_major: 0,
            abi_minor: 0,
            offered: 0,
            selected: 0,
            max_write: 0,
            max_readahead: 0,
            max_background: 0,
            congestion_threshold: 0,
            page_size: 4096,
            loops: 1,
            abort_bound: true,
        },
    }
}
/// Where a failed WinFsp Attach leaves the entry.
enum Stop {
    /// No engine, request or host owner of this attempt remains.
    Unattached(Failure),
    /// The engine mount outcome is unknown: never a second attachment.
    Unknown(Failure),
    /// An engine owner could not be disposed and is kept.
    Retained(Box<WinFspKept>),
}
/// What one bounded host stop answered.
enum StopHost {
    Stopped,
    /// The stop did not finish in the bound; its thread keeps the session
    /// and finishes the teardown whenever the last handle closes.
    Timeout,
    /// No stop thread could be started; the session is back with the caller.
    Unstarted(WinFspSession),
}
/// Stops the host on a thread of its own and waits one [`DETACH_WAIT`] for
/// it: WinFsp's stop waits out every open handle, which a caller's leaked
/// descriptor could otherwise hold forever. Nothing is repeated here.
fn stop_bounded(session: WinFspSession) -> StopHost {
    let held = Arc::new(Mutex::new(Some(session)));
    let worker = held.clone();
    let spawned = std::thread::Builder::new()
        .name("winfsp-detach".into())
        .spawn(move || {
            if let Ok(mut guard) = worker.lock() {
                if let Some(mut session) = guard.take() {
                    session.detach();
                }
            }
        });
    let handle = match spawned {
        Ok(handle) => handle,
        Err(_) => {
            return match Arc::try_unwrap(held) {
                Ok(mutex) => {
                    let mut guard = mutex
                        .into_inner()
                        .unwrap_or_else(|poison| poison.into_inner());
                    match guard.take() {
                        Some(session) => StopHost::Unstarted(session),
                        None => StopHost::Stopped,
                    }
                }
                Err(_) => StopHost::Timeout,
            };
        }
    };
    let deadline = Instant::now() + DETACH_WAIT;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return StopHost::Timeout;
        }
        std::thread::sleep(DETACH_POLL);
    }
    let _ = handle.join();
    StopHost::Stopped
}
/// Terminal admission over a WinFsp-owning entry; the entry observes the
/// session as leaving until the operation settles it.
pub(super) fn take_winfsp(
    kernel: &mut Kernel,
    phase: MountPhase,
) -> Result<Box<WinFspAttached>, Failure> {
    match kernel {
        Kernel::Ready(_) => {}
        Kernel::Leaving(_) => {
            return Err(kernel_failure(
                ControlCode::Busy,
                "unmount:admission",
                "terminal unmount already admitted",
            ));
        }
        Kernel::Unattached | Kernel::Attaching | Kernel::Retained(_) => {
            return Err(kernel_failure(
                ControlCode::Invalid,
                "unmount:admission",
                "Workspace owns no serving WinFsp mount",
            ));
        }
    }
    let Kernel::Ready(attached) = std::mem::replace(kernel, Kernel::Attaching) else {
        unreachable!("checked serving connection");
    };
    *kernel = Kernel::Leaving(Box::new(WinFspLeaving {
        ready: attached.ready.clone(),
        phase,
    }));
    Ok(attached)
}
impl Service {
    /// Attach over WinFsp: Ready is acknowledged only after the host mounts
    /// the engine mount at the Workspace's own directory.
    pub(super) fn attach(
        &self,
        token: WorkspaceToken,
        _timing: &layerfs_telemetry::timer::TimingScope<'_, layerfs_telemetry::timer::Active>,
    ) -> Result<Success, Failure> {
        let Some(serving) = self.kernel.clone() else {
            return Err(Failure::Rejected(
                ControlCode::Invalid,
                "kernel serving not assembled",
            ));
        };
        let workspace = self.admit_winfsp(token)?;
        match self.winfsp_admitted(&serving, token, &workspace) {
            Ok(attached) => {
                let reply = Reply::Ready(Box::new(attached.ready.clone()));
                self.settle(token, Kernel::Ready(Box::new(attached)), Activity::Idle);
                Ok(Success::reply(reply))
            }
            Err(Stop::Unattached(failure)) => {
                self.settle(token, Kernel::Unattached, Activity::Idle);
                Err(failure)
            }
            Err(Stop::Unknown(failure)) => {
                self.settle(token, Kernel::Attaching, Activity::Uncertain);
                Err(failure)
            }
            Err(Stop::Retained(kept)) => {
                let custody = kept.custody(token);
                self.settle(token, Kernel::Retained(kept), Activity::Uncertain);
                Err(Failure::Retained(Box::new(custody)))
            }
        }
    }
    fn admit_winfsp(&self, token: WorkspaceToken) -> Result<Arc<BoundWorkspace>, Failure> {
        let mut entries = self.entries.lock().map_err(|_| Failure::Poisoned)?;
        let value = bound_mut(&mut entries, token)?;
        if let Kernel::Retained(kept) = &value.kernel {
            return Err(Failure::Retained(Box::new(kept.custody(token))));
        }
        idle(value)?;
        if !matches!(value.kernel, Kernel::Unattached) {
            return Err(kernel_failure(
                ControlCode::Invalid,
                "attach:admission",
                "Workspace already owns a kernel connection",
            ));
        }
        value.kernel = Kernel::Attaching;
        value.activity = Activity::Attaching;
        value.epoch = value.epoch.saturating_add(1);
        Ok(value.workspace.clone())
    }
    fn winfsp_admitted(
        &self,
        serving: &WinFspServing,
        token: WorkspaceToken,
        workspace: &Arc<BoundWorkspace>,
    ) -> Result<WinFspAttached, Stop> {
        let directory = serving.directory(token);
        let Some(text) = directory
            .to_str()
            .filter(|text| text.len() <= MOUNT_DIRECTORY_LIMIT)
            .map(str::to_owned)
        else {
            return Err(Stop::Unattached(kernel_failure(
                ControlCode::Invalid,
                "attach:directory",
                "mount directory exceeds the control record bound",
            )));
        };
        let unknown = |failure: Failure| {
            if failure.uncertain() {
                Stop::Unknown(failure)
            } else {
                Stop::Unattached(failure)
            }
        };
        // The same preparation as the FUSE Attach: the root serial, then the
        // initial authoritative serial range before any request can create.
        let operation = workspace
            .operation()
            .map_err(|error| Stop::Unattached(Failure::Workspace(error)))?;
        let root = operation
            .workspace()
            .base()
            .map(|base| base.root().root_inode().serial())
            .map_err(|error| Stop::Unattached(Failure::Workspace(error)))?;
        if let Err(error) = operation.workspace().prepare_serials(operation.ports()) {
            let failure = match operation.ports().failure() {
                Ok(Some(cause)) => match cause.as_ref() {
                    crate::store::PortError::History(error) => Failure::History(error.clone()),
                    _ => Failure::Workspace(error),
                },
                Ok(None) | Err(_) => Failure::Workspace(error),
            };
            return Err(unknown(failure));
        }
        drop(operation);
        // The host creates the mount point itself; a directory already there
        // is a name collision this daemon never adopts.
        if std::fs::symlink_metadata(&directory).is_ok() {
            return Err(Stop::Unattached(kernel_failure(
                ControlCode::Failed,
                "attach:directory",
                format!("mount directory {text} already exists"),
            )));
        }
        let mount = match self.job(workspace.route(), |client: &crate::OwnerClient, route| {
            client.kernel_mount(route, root)
        }) {
            Ok(mount) => mount,
            Err(failure) => return Err(unknown(failure)),
        };
        let queue = serving.register(mount);
        let session = WinFspSession::attach(
            WinFspSessionConfig {
                directory: directory.clone(),
                root,
            },
            queue.clone(),
            Arc::new(crate::service::Mounted(workspace.clone())),
        );
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                // The host removed any mount point it managed to create.
                return Err(self.unwind_winfsp(
                    workspace,
                    mount,
                    kernel_failure(ControlCode::Failed, "attach:mount", error.to_string()),
                ));
            }
        };
        Ok(WinFspAttached {
            session,
            ready: receipt(token, text, &mount, root),
            mount,
            queue,
        })
    }
    /// No request or host owner of this attempt remains: release its engine
    /// mount so the entry returns to Unattached. An unacknowledged release
    /// is kept.
    fn unwind_winfsp(
        &self,
        workspace: &BoundWorkspace,
        mount: KernelMount,
        failure: Failure,
    ) -> Stop {
        match self.job(workspace.route(), |client: &crate::OwnerClient, route| {
            client.kernel_revoke(route, mount)
        }) {
            Ok(()) => Stop::Unattached(failure),
            Err(revoke) => Stop::Retained(Box::new(WinFspKept {
                ready: None,
                stage: TeardownStage::Revoke,
                detail: revoke.wire().detail,
                evidence: Box::new((failure, mount)),
                forced: None,
            })),
        }
    }
    /// Normal terminal unmount of a WinFsp-owning entry, admitted by
    /// [`take_winfsp`]: stop the host (bounded), stop the request service,
    /// revoke the engine mount, then the one logical Close.
    pub(super) fn unmount_winfsp(
        &self,
        token: WorkspaceToken,
        workspace: Arc<BoundWorkspace>,
        attached: Box<WinFspAttached>,
    ) -> Result<Success, Failure> {
        let WinFspAttached {
            session,
            ready,
            mount,
            queue,
        } = *attached;
        self.winfsp_phase(token, MountPhase::Probing);
        match stop_bounded(session) {
            StopHost::Stopped => {}
            StopHost::Unstarted(session) => {
                return Err(self.keep_winfsp(
                    token,
                    TeardownStage::Detach,
                    "detach thread could not be started".into(),
                    Some(ready),
                    None,
                    Box::new(session),
                ));
            }
            StopHost::Timeout => {
                let directory = ready.directory.clone();
                return Err(self.keep_winfsp(
                    token,
                    TeardownStage::Detach,
                    format!("winfsp host did not stop in {DETACH_WAIT:?}; open handles hold it"),
                    Some(ready),
                    None,
                    Box::new(directory),
                ));
            }
        }
        let _ = queue.stop_service();
        self.winfsp_phase(token, MountPhase::Draining);
        if let Err(failure) = self.job(workspace.route(), |client: &crate::OwnerClient, route| {
            client.kernel_revoke(route, mount)
        }) {
            return Err(self.keep_winfsp(
                token,
                TeardownStage::Revoke,
                failure.wire().detail,
                Some(ready),
                None,
                Box::new(mount),
            ));
        }
        if let Ok(mut entries) = self.entries.lock() {
            if let Ok(binding) = bound_mut(&mut entries, token) {
                binding.kernel = Kernel::Unattached;
            }
        }
        self.close(token, &workspace, None)
    }
    /// Forced terminal unmount: the same before-effect guard as FUSE, the
    /// same bounded host stop, then revoke and Close with the forced facts.
    pub(super) fn force_unmount(
        &self,
        token: WorkspaceToken,
        relinquish_unknown: bool,
    ) -> Result<Success, Failure> {
        let (workspace, attached, commit) = {
            let mut entries = self.entries.lock().map_err(|_| Failure::Poisoned)?;
            let value = bound_mut(&mut entries, token)?;
            let commit = super::force::guard(value, token, relinquish_unknown)?;
            let attached = take_winfsp(&mut value.kernel, MountPhase::Stopping)?;
            value.activity = Activity::Closing;
            value.epoch = value.epoch.saturating_add(1);
            (value.workspace.clone(), attached, commit)
        };
        let WinFspAttached {
            session,
            ready,
            mount,
            queue,
        } = *attached;
        let mut facts = ForcedFacts {
            // WinFsp's host stop is the abort control; it is always bound.
            abort: AbortDisposition::Written,
            detach: DetachDisposition::NotAttempted,
            commit,
            fenced: 0,
        };
        match stop_bounded(session) {
            StopHost::Stopped => facts.detach = DetachDisposition::Detached,
            StopHost::Unstarted(session) => {
                facts.detach = DetachDisposition::Failed;
                return Err(self.keep_winfsp(
                    token,
                    TeardownStage::Detach,
                    "forced stop thread could not be started".into(),
                    Some(ready),
                    Some(facts),
                    Box::new(session),
                ));
            }
            StopHost::Timeout => {
                facts.detach = DetachDisposition::Failed;
                let directory = ready.directory.clone();
                return Err(self.keep_winfsp(
                    token,
                    TeardownStage::Detach,
                    format!("winfsp host did not stop in {DETACH_WAIT:?}; open handles hold it"),
                    Some(ready),
                    Some(facts),
                    Box::new(directory),
                ));
            }
        }
        let _ = queue.stop_service();
        self.winfsp_phase(token, MountPhase::Draining);
        if let Err(failure) = self.job(workspace.route(), |client: &crate::OwnerClient, route| {
            client.kernel_revoke(route, mount)
        }) {
            return Err(self.keep_winfsp(
                token,
                TeardownStage::Revoke,
                failure.wire().detail,
                Some(ready),
                Some(facts),
                Box::new(mount),
            ));
        }
        if let Ok(mut entries) = self.entries.lock() {
            if let Ok(binding) = bound_mut(&mut entries, token) {
                binding.kernel = Kernel::Unattached;
            }
        }
        // The mount is released, so its last counters are the zeros the
        // stopped host left.
        let work = MountWork {
            loops_configured: 1,
            loops_entered: 1,
            loops_exited: 1,
            loops_joined: 1,
            active: 0,
            retained: 0,
            completed: 0,
            served: 0,
            inline: 0,
            refused: 0,
            terminal: 0,
            forget_units: 0,
        };
        let kernel = self.closed(token, &workspace, None, Some(facts.clone()))?;
        // One observation of the closed namespace; a failed one claims no
        // state. Its completion is read and dropped here.
        let (cleanup, observation_failure) = match self
            .owner
            .observe_cleanup(None, token.namespace, token.workspace.to_bytes())
            .map_err(Failure::Owner)
        {
            Ok(CleanupState::Held) => (ForcedCleanup::Held, None),
            Ok(CleanupState::Queued) => (ForcedCleanup::Queued, None),
            Ok(CleanupState::Gone) => (ForcedCleanup::Gone, None),
            Ok(CleanupState::Live) => (
                ForcedCleanup::Unobserved,
                Some(Box::new(Failure::Rejected(
                    ControlCode::Unknown,
                    "closed Workspace remains live",
                ))),
            ),
            Err(error) => (ForcedCleanup::Unobserved, Some(Box::new(error))),
        };
        self.depart(
            token,
            Success {
                reply: Reply::ForceUnmounted(Box::new(ForceUnmounted {
                    token,
                    outcome: ForcedOutcome {
                        facts,
                        work,
                        cleanup,
                    },
                })),
                commit: None,
                observation_failure,
                kernel,
            },
        )
    }
    /// Stops a terminal operation after effects, keeping its owner in the
    /// entry with the facts as they stood.
    fn keep_winfsp(
        &self,
        token: WorkspaceToken,
        stage: TeardownStage,
        detail: String,
        ready: Option<ReadyMount>,
        forced: Option<ForcedFacts>,
        evidence: Box<dyn fmt::Debug + Send>,
    ) -> Failure {
        let kept = WinFspKept {
            ready,
            stage,
            detail,
            evidence,
            forced,
        };
        let custody = kept.custody(token);
        self.settle(token, Kernel::Retained(Box::new(kept)), Activity::Uncertain);
        Failure::Retained(Box::new(custody))
    }
    /// Advances the leaving phase of a terminal operation's entry.
    fn winfsp_phase(&self, token: WorkspaceToken, phase: MountPhase) {
        if let Ok(mut entries) = self.entries.lock() {
            if let Ok(binding) = bound_mut(&mut entries, token) {
                if let Kernel::Leaving(leaving) = &mut binding.kernel {
                    leaving.phase = phase;
                }
            }
        }
    }
}
