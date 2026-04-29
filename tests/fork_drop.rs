mod common;

use std::path::PathBuf;

use forkspace::error::ForkspaceError;
use forkspace::state::State;
use serial_test::serial;

#[test]
#[serial]
fn test_fork_creates_workspace() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = common::fork_helper(base.path().to_path_buf(), "test-ws").unwrap();

    assert!(ws.upper_path().unwrap().exists());
    assert!(ws.work_path().unwrap().exists());
    assert!(ws.view_path.exists());

    let state = State::load().unwrap();
    assert!(state.workspaces.contains_key("test-ws"));

    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn test_fork_merged_shows_files() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = common::fork_helper(base.path().to_path_buf(), "files-ws").unwrap();

    assert!(ws.view_path.join("README.md").exists());
    assert!(ws.view_path.join("src/main.rs").exists());
    assert!(ws.view_path.join("Cargo.toml").exists());

    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn test_fork_duplicate_name() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = common::fork_helper(base.path().to_path_buf(), "dup-ws").unwrap();

    let result = common::fork_helper(base.path().to_path_buf(), "dup-ws");
    assert!(matches!(
        result.unwrap_err(),
        ForkspaceError::WorkspaceExists(_)
    ));

    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn test_fork_nonexistent_base() {
    let _dir = common::setup_test_env();
    let result = common::fork_helper(PathBuf::from("/tmp/does-not-exist-forkspace-test"), "bad-ws");
    assert!(result.is_err());
}

#[test]
#[serial]
fn test_fork_stacked() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws1 = common::fork_helper(base.path().to_path_buf(), "parent-ws").unwrap();
    let ws2 = common::fork_helper(PathBuf::from("parent-ws"), "child-ws").unwrap();

    assert_eq!(ws2.parent.as_deref(), Some("parent-ws"));
    assert_eq!(ws2.base, ws1.view_path.canonicalize().unwrap());

    common::cleanup_workspace(&ws2);
    common::cleanup_workspace(&ws1);
}

#[test]
#[serial]
fn test_fork_writes_state() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = forkspace::workspace::create(
        base.path().to_path_buf(),
        "state-ws",
        forkspace::workspace::CreateOptions {
            session: Some("my-session".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let state = State::load().unwrap();
    let stored = state.workspaces.get("state-ws").unwrap();
    assert_eq!(stored.name, "state-ws");
    assert_eq!(stored.session.as_deref(), Some("my-session"));
    assert!(stored.base.exists());

    common::cleanup_workspace(&ws);
}

// ── Drop tests ──

#[test]
#[serial]
fn test_drop_workspace() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = common::fork_helper(base.path().to_path_buf(), "drop-ws").unwrap();
    let ws_dir = forkspace::paths::forkspace_home().join("drop-ws");
    assert!(ws_dir.exists());

    forkspace::commands::drop::exec(forkspace::cli::DropArgs {
        name: Some("drop-ws".to_string()),
        force: false,
        all: false,
        glob: false,
    })
    .unwrap();

    assert!(!ws_dir.exists());
    let state = State::load().unwrap();
    assert!(!state.workspaces.contains_key("drop-ws"));
    let _ = ws;
}

#[test]
#[serial]
fn test_drop_nonexistent() {
    let _dir = common::setup_test_env();
    let result = forkspace::commands::drop::exec(forkspace::cli::DropArgs {
        name: Some("nonexistent-ws".to_string()),
        force: false,
        all: false,
        glob: false,
    });
    assert!(matches!(
        result.unwrap_err(),
        ForkspaceError::WorkspaceNotFound(_)
    ));
}

#[test]
#[serial]
fn test_drop_with_children_requires_force() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws1 = common::fork_helper(base.path().to_path_buf(), "parent-drop").unwrap();
    let ws2 = common::fork_helper(PathBuf::from("parent-drop"), "child-drop").unwrap();

    let result = forkspace::commands::drop::exec(forkspace::cli::DropArgs {
        name: Some("parent-drop".to_string()),
        force: false,
        all: false,
        glob: false,
    });
    match result.unwrap_err() {
        ForkspaceError::HasChildren {
            workspace,
            children,
        } => {
            assert_eq!(workspace, "parent-drop");
            assert!(children.contains(&"child-drop".to_string()));
        }
        other => panic!("expected HasChildren, got: {:?}", other),
    }

    let state = State::load().unwrap();
    assert!(state.workspaces.contains_key("parent-drop"));
    assert!(state.workspaces.contains_key("child-drop"));

    common::cleanup_workspace(&ws2);
    common::cleanup_workspace(&ws1);
}

#[test]
#[serial]
fn test_drop_force_cascade() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let _ws1 = common::fork_helper(base.path().to_path_buf(), "cascade-parent").unwrap();
    let _ws2 = common::fork_helper(PathBuf::from("cascade-parent"), "cascade-child").unwrap();

    forkspace::commands::drop::exec(forkspace::cli::DropArgs {
        name: Some("cascade-parent".to_string()),
        force: true,
        all: false,
        glob: false,
    })
    .unwrap();

    let state = State::load().unwrap();
    assert!(!state.workspaces.contains_key("cascade-parent"));
    assert!(!state.workspaces.contains_key("cascade-child"));
}

// ── Ls tests ──

#[test]
#[serial]
fn test_ls_empty() {
    let _dir = common::setup_test_env();
    forkspace::commands::ls::exec(forkspace::cli::LsArgs {
        names: false,
        json: false,
        no_gitignore: false,
    })
    .unwrap();
}

#[test]
#[serial]
fn test_ls_shows_workspaces() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = common::fork_helper(base.path().to_path_buf(), "ls-test-ws").unwrap();
    forkspace::commands::ls::exec(forkspace::cli::LsArgs {
        names: false,
        json: false,
        no_gitignore: false,
    })
    .unwrap();

    let state = State::load().unwrap();
    assert!(state.workspaces.contains_key("ls-test-ws"));

    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn test_fork_tmpfs_with_overlay() {
    let (_dir, base) = common::setup_with_project();

    let result = forkspace::workspace::create(
        base.path().to_path_buf(),
        "tmpfs-overlay-ws",
        forkspace::workspace::CreateOptions {
            tmpfs: true,
            tmpfs_size: Some("128m".to_string()),
            ..Default::default()
        },
    );

    assert!(matches!(
        result,
        Err(ForkspaceError::CapabilityUnsupported { .. })
    ));
    assert!(State::load().unwrap().workspaces.is_empty());
}

#[test]
#[serial]
fn test_fork_rejects_unsafe_workspace_names() {
    let (_dir, base) = common::setup_with_project();
    for name in ["", ".", "..", "a/b", "a\0b"] {
        let error = common::fork_helper(base.path().to_path_buf(), name).unwrap_err();
        assert!(
            matches!(error, ForkspaceError::InvalidArgument(_)),
            "accepted {name:?}"
        );
    }

    let long_name = "x".repeat(256);
    let error = common::fork_helper(base.path().to_path_buf(), &long_name).unwrap_err();
    assert!(matches!(error, ForkspaceError::InvalidArgument(_)));
}

#[test]
#[serial]
fn test_fork_stack_depth_is_bounded() {
    let _home = common::setup_test_env();
    let mut state = State::default();
    for index in 0..32 {
        let parent = (index > 0).then(|| format!("depth-{}", index - 1));
        state
            .add_workspace(forkspace::workspace::Workspace::new(
                &format!("depth-{index}"),
                PathBuf::from("/tmp/base"),
                parent,
            ))
            .unwrap();
    }
    state.save().unwrap();

    let error = forkspace::workspace::create(PathBuf::from("depth-31"), "too-deep", Default::default())
        .unwrap_err();
    assert!(matches!(
        error,
        forkspace::error::ForkspaceError::InvalidArgument(_)
    ));
    assert!(error.to_string().contains("stack depth"));
}
