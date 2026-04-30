mod common;

use std::path::PathBuf;

use forkspace::error::ForkspaceError;
use forkspace::state::State;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn install_state_fixture(dir: &tempfile::TempDir, name: &str) {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::copy(fixture, dir.path().join("state.json")).unwrap();
}

fn make_workspace(name: &str, parent: Option<String>) -> Workspace {
    let base = PathBuf::from("/tmp/base");
    let root = forkspace::paths::forkspace_home();
    common::make_test_workspace(&root, name, &base, parent)
}

#[test]
#[serial]
fn test_load_empty() {
    let _dir = common::setup_test_env();
    let state = State::load().unwrap();
    assert_eq!(state.version, 2);
    assert!(state.workspaces.is_empty());
}

#[test]
#[serial]
fn test_add_and_get_workspace() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    let ws = make_workspace("test-ws", None);
    state.add_workspace(ws).unwrap();

    let ws = state.workspaces.get("test-ws");
    assert!(ws.is_some());
    assert_eq!(ws.unwrap().name, "test-ws");
}

#[test]
#[serial]
fn test_add_duplicate() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    state.add_workspace(make_workspace("dup", None)).unwrap();
    let result = state.add_workspace(make_workspace("dup", None));
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ForkspaceError::WorkspaceExists(_)
    ));
}

#[test]
#[serial]
fn test_remove_workspace() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    state.add_workspace(make_workspace("rm-me", None)).unwrap();
    assert!(state.workspaces.contains_key("rm-me"));

    state.remove_workspace("rm-me").unwrap();
    assert!(!state.workspaces.contains_key("rm-me"));
}

#[test]
#[serial]
fn test_remove_nonexistent() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    let result = state.remove_workspace("nope");
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ForkspaceError::WorkspaceNotFound(_)
    ));
}

#[test]
#[serial]
fn test_list_workspaces() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    state.add_workspace(make_workspace("a", None)).unwrap();
    state.add_workspace(make_workspace("b", None)).unwrap();
    state.add_workspace(make_workspace("c", None)).unwrap();

    let list = state.workspaces.values().collect::<Vec<_>>();
    assert_eq!(list.len(), 3);

    let mut names: Vec<&str> = list.iter().map(|ws| ws.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
#[serial]
fn test_update_workspace() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    state.add_workspace(make_workspace("upd", None)).unwrap();

    let ws = state.workspaces.get_mut("upd").unwrap();
    if let forkspace::backend::BackendState::Overlay(backend) = &mut ws.backend_state {
        backend.tmpfs = true;
    }

    let ws = state.workspaces.get("upd").unwrap();
    assert!(ws.tmpfs());
}

#[test]
#[serial]
fn test_corrupted_json() {
    let dir = common::setup_test_env();
    let state_path = dir.path().join("state.json");
    std::fs::write(&state_path, "{{not valid json!!!").unwrap();

    let result = State::load();
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ForkspaceError::StateCorrupted(_)
    ));
}

#[test]
#[serial]
fn test_empty_file() {
    let dir = common::setup_test_env();
    let state_path = dir.path().join("state.json");
    std::fs::write(&state_path, "").unwrap();

    let state = State::load().unwrap();
    assert_eq!(state.version, 2);
    assert!(state.workspaces.is_empty());
}

#[test]
#[serial]
fn test_children_of() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    state.add_workspace(make_workspace("root", None)).unwrap();
    state
        .add_workspace(make_workspace("child1", Some("root".into())))
        .unwrap();
    state
        .add_workspace(make_workspace("child2", Some("root".into())))
        .unwrap();
    state.add_workspace(make_workspace("other", None)).unwrap();

    let children = state.children_of("root");
    assert_eq!(children.len(), 2);

    let mut names: Vec<&str> = children.iter().map(|ws| ws.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["child1", "child2"]);

    assert!(state.children_of("other").is_empty());
}

#[test]
#[serial]
fn test_parent_chain() {
    let _dir = common::setup_test_env();
    let mut state = State::load().unwrap();
    state
        .add_workspace(make_workspace("grandparent", None))
        .unwrap();
    state
        .add_workspace(make_workspace("parent", Some("grandparent".into())))
        .unwrap();
    state
        .add_workspace(make_workspace("child", Some("parent".into())))
        .unwrap();

    let chain = state.parent_chain("child");
    let names: Vec<&str> = chain.iter().map(|ws| ws.name.as_str()).collect();
    assert_eq!(names, vec!["grandparent", "parent", "child"]);

    let chain = state.parent_chain("grandparent");
    let names: Vec<&str> = chain.iter().map(|ws| ws.name.as_str()).collect();
    assert_eq!(names, vec!["grandparent"]);
}

#[test]
#[serial]
fn test_save_creates_dirs() {
    let dir = common::setup_test_env();
    let nested = dir.path().join("a").join("b").join("c");
    // SAFETY: this test is serialized with #[serial], so no concurrent
    // env var mutation can occur.
    unsafe { std::env::set_var("FORKSPACE_HOME", &nested) };

    let state = State::default();
    state.save().unwrap();

    assert!(nested.join("state.json").exists());
}

#[test]
#[serial]
fn test_state_persistence() {
    let _dir = common::setup_test_env();

    {
        let mut state = State::load().unwrap();
        state
            .add_workspace(make_workspace("persist", None))
            .unwrap();
        state
            .add_workspace(make_workspace("persist-child", Some("persist".into())))
            .unwrap();
        state.save().unwrap();
    }

    {
        let state = State::load().unwrap();
        assert_eq!(state.workspaces.len(), 2);
        let ws = state.workspaces.get("persist").unwrap();
        assert_eq!(ws.name, "persist");
        let child = state.workspaces.get("persist-child").unwrap();
        assert_eq!(child.parent.as_deref(), Some("persist"));
    }
}

#[test]
#[serial]
fn test_state_v1_fixture_round_trips_without_loss() {
    let dir = common::setup_test_env();
    install_state_fixture(&dir, "state-v1.json");
    let state = State::load().unwrap();
    state.save().unwrap();

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(after["version"], 2);
    assert_eq!(after["workspaces"]["fixture"]["name"], "fixture");
    assert_eq!(after["workspaces"]["fixture"]["session"], "fixture-session");
    assert_eq!(after["workspaces"]["fixture"]["backend_kind"], "linux");
    assert!(dir.path().join("state.v1.bak").exists());
}

#[test]
#[serial]
fn test_unknown_state_fields_survive_load_and_save() {
    let dir = common::setup_test_env();
    install_state_fixture(&dir, "state-v1-with-unknown-fields.json");

    let state = State::load().unwrap();
    state.save().unwrap();

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(saved["future_field"], serde_json::json!({"enabled": true}));
}

#[test]
#[serial]
fn test_newer_state_is_never_overwritten() {
    let dir = common::setup_test_env();
    let path = dir.path().join("state.json");
    let original = b"{\"version\":99,\"workspaces\":{},\"future\":\"keep\"}\n";
    std::fs::write(&path, original).unwrap();

    let result = State::with_state(|state| {
        state.proxy = None;
        Ok(())
    });

    assert!(matches!(result, Err(ForkspaceError::StateFailed(_))));
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
#[serial]
fn test_trash_is_recorded_and_retried_on_load() {
    let dir = common::setup_test_env();
    let orphan = dir.path().join("trash/orphan");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("file"), b"trash").unwrap();
    State::default().save().unwrap();

    let state = State::load().unwrap();
    assert!(state.trash.iter().any(|record| record.path == orphan));

    for _ in 0..100 {
        if !orphan.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!orphan.exists());
    assert!(State::load().unwrap().trash.is_empty());
}

#[test]
#[serial]
fn test_orphan_manifest_is_removed_during_reconciliation() {
    let dir = common::setup_test_env();
    let orphan = dir.path().join("manifests/orphan.jsonl");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"{}\n").unwrap();
    State::default().save().unwrap();

    State::load().unwrap();
    assert!(!orphan.exists());
}

#[test]
#[serial]
fn forkspace_home_is_created_private_and_tightened() {
    use std::os::unix::fs::PermissionsExt;

    let parent = tempfile::tempdir().unwrap();
    let home = parent.path().join("fresh-home");
    unsafe { std::env::set_var("FORKSPACE_HOME", &home) };
    State::load().unwrap();
    let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);

    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    State::load().unwrap();
    let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}
