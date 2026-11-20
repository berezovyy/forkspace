mod common;

use forkspace::error::ForkspaceError;
use forkspace::paths::forkspace_home;
use forkspace::state::State;
use serial_test::serial;

fn add_fake_workspace(state: &mut State, name: &str, parent: Option<String>) {
    let home = forkspace_home();
    let fake_base = std::path::PathBuf::from("/tmp/fake-base");
    let mut ws = common::make_test_workspace(&home, name, &fake_base, parent);
    // Only Linux builds open overlay workspaces; elsewhere record a plain directory copy.
    if !cfg!(target_os = "linux") {
        ws.view_path = forkspace::paths::snapshot_dir(name);
        std::fs::create_dir_all(&ws.view_path).unwrap();
        ws.backend_kind = forkspace::backend::BackendKind::PlainCopy;
        ws.backend_state = forkspace::backend::BackendState::CowDir {
            reflinked_files: 0,
            copied_files: 0,
            unknown_fields: Default::default(),
        };
    }
    state.add_workspace(ws).unwrap();
}

// ── Drop tests ──

#[test]
#[serial]
fn drop_takes_several_names() {
    let _dir = common::setup_test_env();

    let mut state = State::default();
    add_fake_workspace(&mut state, "all-ws-1", None);
    add_fake_workspace(&mut state, "all-ws-2", None);
    add_fake_workspace(&mut state, "all-ws-3", None);
    state.save().unwrap();

    forkspace::commands::drop::exec(forkspace::cli::DropArgs {
        names: vec![
            "all-ws-1".to_string(),
            "all-ws-2".to_string(),
            "all-ws-3".to_string(),
        ],
    })
    .unwrap();

    let state = State::load().unwrap();
    assert!(state.workspaces.values().collect::<Vec<_>>().is_empty());
}

// ── Error message tests ──

#[test]
fn test_error_messages_are_helpful() {
    let err = ForkspaceError::WorkspaceNotFound("my-ws".to_string());
    let msg = format!("{}", err);
    assert!(msg.contains("my-ws"));
    assert!(msg.contains("forkspace ls"));

    let err = ForkspaceError::WorkspaceExists("my-ws".to_string());
    let msg = format!("{}", err);
    assert!(msg.contains("my-ws"));
    assert!(msg.contains("forkspace enter"));
}
