//! Bare-run WinFsp Workspace runner: init/open Store, bind a Workspace,
//! mount it with the WinFsp backend, run one command at the mount, commit.
//!
//! No Sandbox, no Docker: the command runs as this user on the host with the
//! mount as its working directory (the trust model recorded in WINFSP-PORT.md).
//!
//! Usage: `winfsp_run <run_dir> [--idle <seconds>] -- <command> [args...]`
//!
//! `<run_dir>` keeps `store.sqlite` (created empty on first run, reused
//! after) and the transient `overlay.sqlite` and `mount/` directory.
//! `--idle` holds the mount for `<seconds>` after the command ends (or with
//! no command at all, default 10 s) before committing and unmounting; it is
//! how an external suite gets a live volume to work against.

/// Progress lines survive a reader that closed early (a test harness stops
/// reading once it saw `mounted`): a broken pipe ends the line, not the run.
fn say(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn complain(text: &str) {
    use std::io::Write;
    let mut out = std::io::stderr();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}
#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use layerfs_daemon::{self, Mounted};
    use layerfs_fuse::{winfsp::WinFspSessionConfig, MountRequests};
    use layerfs_history::{
        BranchId, ForkRequest, ForkSource, HistoryCatalog, HistoryCatalogConfig, HistoryName,
        LayerStackId, WorkspaceId,
    };
    use layerfs_overlay::ProfileConfig;
    use layerfs_persistence::{
        Handles, PersistenceConfig, SqliteAcquisitionSchema, SqlitePersistenceProfile,
    };
    use layerfs_project::{init, InitRequest};
    use layerfs_storage::{Storage, StoragePolicy};
    use layerfs_store_service::{store::BindRequest, Owner, OwnerConfig};
    use std::{
        ffi::OsStr,
        process::Command,
        sync::Arc,
        time::{Duration, Instant},
    };

    const BINDING: &[u8] = b"winfsp-run-store";
    const CURSOR: [u8; 32] = [43; 32];
    const BRANCH: [u8; 16] = [24; 16];
    const STACK: [u8; 16] = [23; 16];

    let usage = "usage: winfsp_run <run_dir> [--idle <seconds>] -- <command> [args...]";
    let mut args = std::env::args_os().skip(1);
    let Some(run_dir) = args.next() else {
        complain(usage);
        return std::process::ExitCode::FAILURE;
    };
    let mut idle: Option<u64> = None;
    let mut separator = args.next();
    if separator.as_deref() == Some(OsStr::new("--idle")) {
        let Some(seconds) = args.next() else {
            complain(usage);
            return std::process::ExitCode::FAILURE;
        };
        let Some(parsed) = seconds.to_str().and_then(|text| text.parse().ok()) else {
            complain("winfsp_run: --idle wants a number of seconds");
            return std::process::ExitCode::FAILURE;
        };
        idle = Some(parsed);
        separator = args.next();
    }
    let program = match separator {
        // No `--` and nothing after the run dir (or after `--idle`): a
        // no-command run that just holds the mount.
        None => None,
        Some(seen) if seen == OsStr::new("--") => args.next(),
        Some(_) => {
            complain(usage);
            return std::process::ExitCode::FAILURE;
        }
    };
    let rest: Vec<_> = args.collect();
    // With no command the mount is held for the idle window (10 s by
    // default); with one, only an explicit --idle holds it after the exit.
    let hold = idle.unwrap_or(if program.is_none() { 10 } else { 0 });
    let run_dir = std::path::PathBuf::from(run_dir);
    std::fs::create_dir_all(&run_dir).expect("create run dir");
    let store_path = run_dir.join("store.sqlite");
    // One Workspace per run: a previous run's overlay is disposable.
    let _ = std::fs::remove_file(run_dir.join("overlay.sqlite"));
    let _ = std::fs::remove_file(run_dir.join("1.sqlite"));

    // First run: an empty sealed Store with one `main` branch.
    if !store_path.exists() {
        let source = run_dir.join("source");
        std::fs::create_dir(&source).expect("create source");
        let sealed = run_dir.join("sealed.sqlite");
        let config = PersistenceConfig::sqlite(&sealed)
            .with_sqlite_profile(SqlitePersistenceProfile::Disposable)
            .with_sqlite_acquisition(SqliteAcquisitionSchema::Tables);
        let handles = Handles::create(
            config,
            StoragePolicy::frozen_default(),
            &HistoryCatalogConfig {
                binding_key: BINDING.to_vec(),
                cursor_key: CURSOR,
                incarnation: 1,
            },
        )
        .expect("create sealed store");
        let storage = Storage::new(handles.storage.clone()).expect("storage");
        let initialized = layerfs_telemetry::timer::Timing::disabled("run.init", |timing| {
            init(
                &storage,
                &handles.history,
                InitRequest {
                    source: &source,
                    acquisition: &handles.acquisition,
                    stack: LayerStackId::from_authority(STACK),
                    name: HistoryName::new("winfsp-run").unwrap(),
                    scope_seed: [31; 32],
                    deadline: Instant::now() + Duration::from_secs(60),
                },
                timing,
            )
        })
        .0
        .expect("init empty project");
        handles
            .history
            .fork(&ForkRequest {
                stack: initialized.stack.id,
                branch: BranchId::from_authority(BRANCH),
                name: HistoryName::new("main").unwrap(),
                source: ForkSource::Layer(initialized.stack.head_layer),
            })
            .expect("fork main");
        drop(storage);
        let sealed = handles.seal().expect("seal");
        std::fs::copy(sealed.path, &store_path).expect("install store copy");
        std::fs::remove_dir_all(&source).ok();
        std::fs::remove_file(&run_dir.join("sealed.sqlite")).ok();
        say(&format!(
            "initialized empty store at {}",
            store_path.display()
        ));
    }

    // Overlay owner and the installed Store.
    let started = Owner::start_observed(
        &run_dir.join("overlay.sqlite"),
        ProfileConfig {
            pager_kib: 1024,
            max_pages: None,
        },
        OwnerConfig { namespaces: 8 },
    );
    let owner = started.result.expect("overlay owner");
    let store = layerfs_store_service::bootstrap::open_store(
        PersistenceConfig::sqlite(&store_path)
            .with_sqlite_profile(SqlitePersistenceProfile::Disposable)
            .with_sqlite_acquisition(SqliteAcquisitionSchema::Tables),
        BINDING,
        CURSOR,
        8,
        64 * 1024,
        layerfs_storage::ReservationBlocks::default(),
    )
    .expect("open store");

    // Bind one Workspace on main, prepare serials, mint the kernel mount.
    let workspace_id = WorkspaceId::from_authority([1; 32]).unwrap();
    let bound = store
        .bind(
            owner.client(),
            BindRequest {
                branch: BranchId::from_authority(BRANCH),
                workspace: workspace_id,
            },
        )
        .expect("bind workspace")
        .workspace;
    let bound = Arc::new(bound);
    let operation = bound.operation().expect("operation");
    let root = operation
        .workspace()
        .base()
        .expect("base")
        .root()
        .root_inode()
        .serial();
    operation
        .workspace()
        .prepare_serials(operation.ports())
        .expect("prepare serials");
    drop(operation);
    let mount = owner
        .client()
        .kernel_mount(bound.route(), root)
        .expect("kernel mount");

    // Mount with the WinFsp backend, run the command bare, then commit.
    let queue = MountRequests::new(mount);
    let mount_dir = run_dir.join("mount");
    let mut session = layerfs_fuse::winfsp::WinFspSession::attach(
        WinFspSessionConfig {
            directory: mount_dir.clone(),
            root,
        },
        queue.clone(),
        Arc::new(Mounted(bound.clone())),
    )
    .expect("winfsp attach");
    say(&format!("mounted at {}", mount_dir.display()));

    let status = match &program {
        Some(program) => Command::new(program)
            .args(&rest)
            .current_dir(&mount_dir)
            .status(),
        None => Ok(std::process::ExitStatus::default()),
    };
    if hold > 0 {
        say(&format!("holding the mount for {hold} s"));
        std::thread::sleep(Duration::from_secs(hold));
    }

    match bound.commit_captured(&layerfs_store_service::store::CommitOptions {
        force: false,
        message: Some("winfsp-run".to_owned()),
    }) {
        Ok(_) => say("committed"),
        Err(failure) => complain(&format!("commit failed: {failure}")),
    }
    session.detach();
    let _ = queue.stop_service();
    say("unmounted");

    match status {
        Ok(status) if status.success() => std::process::ExitCode::SUCCESS,
        Ok(status) => {
            complain(&format!("command failed: {status}"));
            std::process::ExitCode::FAILURE
        }
        Err(error) => {
            complain(&format!("command failed to start: {error}"));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    complain("winfsp_run is Windows-only");
    std::process::exit(1);
}
