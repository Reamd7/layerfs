//! In-process daemon control-plane coverage on Windows: Mount → Attach
//! (WinFsp) → the mount directory is a live volume → Unmount → the directory
//! is gone → a fresh Mount/Attach works again. No TCP, no Sandbox: the
//! `Service` methods are called directly, exactly as `execute_control`
//! dispatches them. Ignored by default: it mounts a real volume
//! (`cargo test --test winfsp_control -- --ignored`).
#![cfg(windows)]
use layerfs_bridge::control::{Reply, Request, UnmountControl};
use layerfs_daemon::control::{KernelConfig, KernelServing, Service};
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
use layerfs_store_service::{bootstrap::open_store, Owner, OwnerConfig};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const BINDING: &[u8] = b"winfsp-control-test";
const CURSOR: [u8; 32] = [47; 32];
const BRANCH: [u8; 16] = [25; 16];
const STACK: [u8; 16] = [22; 16];

fn run_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("layerfs_winfsp_control_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

/// One empty sealed Store with a `main` branch, installed at `path`.
fn installed_store(path: &std::path::Path, source: &std::path::Path) {
    let sealed_path = path.with_extension("sealed.sqlite");
    let config = PersistenceConfig::sqlite(&sealed_path)
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
    let initialized = layerfs_telemetry::timer::Timing::disabled("control.init", |timing| {
        init(
            &storage,
            &handles.history,
            InitRequest {
                source,
                acquisition: &handles.acquisition,
                stack: LayerStackId::from_authority(STACK),
                name: HistoryName::new("winfsp-control").unwrap(),
                scope_seed: [33; 32],
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
    std::fs::copy(&sealed.path, path).expect("install store copy");
    drop(sealed);
    let _ = std::fs::remove_file(&sealed_path);
}

#[test]
#[ignore = "needs WinFsp driver"]
fn attach_unmount_attach_through_control() {
    let dir = run_dir("cycle");
    let source = dir.join("source");
    std::fs::create_dir(&source).expect("create source");
    let store_path = dir.join("store.sqlite");
    installed_store(&store_path, &source);
    std::fs::remove_dir_all(&source).ok();
    let mounts = dir.join("mounts");

    let started = Owner::start_observed(
        &dir.join("overlay.sqlite"),
        ProfileConfig {
            pager_kib: 1024,
            max_pages: None,
        },
        OwnerConfig { namespaces: 8 },
    );
    let owner = started.result.expect("overlay owner");
    let store = open_store(
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
    let serving = Arc::new(
        KernelServing::start(KernelConfig {
            mounts: mounts.clone(),
            owner_uid: 0,
            owner_gid: 0,
            command_uid: 0,
            command_gid: 0,
            ready_wait: Duration::from_secs(5),
            drain_wait: Duration::from_secs(5),
            control: "/sys/fs/fuse/connections".into(),
        })
        .expect("start winfsp serving"),
    );
    let service = Service::with_kernel(store, &owner, serving);

    let workspace = WorkspaceId::from_authority([7; 32]).unwrap();
    let token = {
        let success = service
            .execute_control(&Request::Mount {
                workspace,
                branch: BranchId::from_authority(BRANCH),
            })
            .expect("mount binds the workspace");
        let Reply::Bound { token, .. } = success.reply else {
            panic!("mount did not answer Bound: {:?}", success.reply);
        };
        token
    };

    // Attach: Ready carries the directory, which is now a live volume.
    let attached = service
        .execute_control(&Request::Attach(token))
        .expect("attach mounts over winfsp");
    let (directory, receipt) = match &attached.reply {
        Reply::Ready(ready) => (ready.directory.clone(), ready.receipt.clone()),
        other => panic!("attach did not answer Ready: {other:?}"),
    };
    assert_eq!(receipt.abi_major, 0, "a WinFsp receipt carries abi 0");
    let listed = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("mount directory readable: {error}"))
        .count();
    assert_eq!(listed, 0, "the empty committed root lists nothing");

    // Unmount: the mount directory is gone, the binding is retired.
    let unmounted = service
        .execute_control(&Request::Unmount(UnmountControl {
            token,
            discard: true,
        }))
        .expect("unmount stops the host");
    assert!(matches!(unmounted.reply, Reply::Unmounted(seen) if seen == token));
    assert!(
        !std::path::Path::new(&directory).exists(),
        "mount directory removed"
    );

    // A fresh incarnation mounts again on the same Store and mounts home.
    let token = {
        let success = service
            .execute_control(&Request::Mount {
                // A fresh authority incarnation, as a real caller selects.
                workspace: WorkspaceId::from_authority([8; 32]).unwrap(),
                branch: BranchId::from_authority(BRANCH),
            })
            .expect("second mount binds again");
        let Reply::Bound { token, .. } = success.reply else {
            panic!("second mount did not answer Bound");
        };
        token
    };
    let attached = service
        .execute_control(&Request::Attach(token))
        .expect("second attach mounts over winfsp");
    assert!(matches!(attached.reply, Reply::Ready(_)));
    let finished = service.execute_control(&Request::Unmount(UnmountControl {
        token,
        discard: true,
    }));
    assert!(finished.is_ok(), "final unmount: {finished:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
