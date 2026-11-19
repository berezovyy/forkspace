mod common;

use std::fs;

use forkspace::backend::overlay::OverlayBackend;
use forkspace::backend::{BackendKind, BackendState, DestroyMode, OverlayState, WorkspaceBackend};
use forkspace::error::ForkspaceError;
use forkspace::workspace::Workspace;
use serial_test::serial;

/// Every entry forkspace keeps directly in `FORKSPACE_HOME`, where an overlay workspace's
/// root also lives (`FORKSPACE_HOME/<name>`).
const STORE_ENTRIES: [&str; 15] = [
    "state.json",
    "state.lock",
    "workspaces",
    "fuse-overlayfs-probe",
    "frozen",
    "ancestors",
    "checkpoints",
    "undo",
    "trash",
    "trash-locks",
    "journal",
    "manifests",
    "logs",
    "proxy.log",
    "proxy.err",
];

#[test]
#[serial]
fn store_names_are_not_workspace_names() {
    // SEC-02: dropping an overlay workspace named `workspaces` deleted every snapshot
    // workspace; `manifests`, `frozen`, `trash`, ... deleted the shared stores.
    let (home, base) = common::setup_with_project();
    let mut names: Vec<String> = STORE_ENTRIES.iter().map(|name| name.to_string()).collect();
    names.extend(
        [
            "Workspaces",
            "TRASH",
            "state.json.tmp",
            "fuse-overlayfs-probe.tmp-42",
        ]
        .map(String::from),
    );
    // Hidden names are where forkspace keeps temporary and staging files.
    names.extend([".hidden", ".a.jsonl.tmp-1"].map(String::from));
    for name in &names {
        let error =
            forkspace::workspace::create(base.path().to_path_buf(), name, Default::default())
                .unwrap_err();
        assert!(
            matches!(error, ForkspaceError::InvalidArgument(_)),
            "{name}: {error}"
        );
    }
    let state = forkspace::state::State::load().unwrap();
    assert!(state.workspaces.is_empty());
    assert!(!home.path().join("workspaces").join("Workspaces").exists());
    // Names that only resemble a store entry stay usable.
    for name in ["workspaces-2", "my.trash", "logs2"] {
        forkspace::workspace::create(base.path().to_path_buf(), name, Default::default()).unwrap();
    }
}

#[test]
#[serial]
fn recovering_an_abandoned_create_named_like_a_store_keeps_the_store() {
    // An interrupted create's directory is moved to the trash on the next state load; for a
    // pending create recorded before names were checked, that directory is a store.
    let home = common::setup_test_env();
    let store = home.path().join("workspaces");
    fs::create_dir_all(store.join("other-workspace")).unwrap();
    fs::write(store.join("other-workspace/file"), b"someone else's\n").unwrap();
    let state = serde_json::json!({
        "version": 2,
        "workspaces": {},
        "pending_creates": [{
            "name": "workspaces",
            "path": store,
            "backend_kind": "linux-overlay",
            "pid": 999_999_999u32,
        }],
    });
    fs::write(home.path().join("state.json"), state.to_string()).unwrap();

    forkspace::state::State::with_state(|_| Ok(())).unwrap();
    assert_eq!(
        fs::read(store.join("other-workspace/file")).unwrap(),
        b"someone else's\n"
    );
}

#[test]
#[serial]
fn dropping_an_overlay_record_named_like_a_store_keeps_the_store() {
    // A record made before names were checked must not take the store down with it.
    let home = common::setup_test_env();
    for name in ["workspaces", "manifests", "trash"] {
        let store = home.path().join(name);
        fs::create_dir_all(store.join("other-workspace")).unwrap();
        fs::write(store.join("other-workspace/file"), b"someone else's\n").unwrap();
        let mut ws = Workspace::new(name, home.path().to_path_buf(), None);
        ws.backend_kind = BackendKind::Overlay;
        let layers = OverlayState::at(&store);
        ws.view_path = layers.merged.clone();
        ws.backend_state = BackendState::Overlay(layers);

        OverlayBackend::new()
            .destroy(&ws, DestroyMode::Normal)
            .unwrap();
        assert_eq!(
            fs::read(store.join("other-workspace/file")).unwrap(),
            b"someone else's\n",
            "{name}"
        );
    }
}
