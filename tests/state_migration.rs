mod common;

use std::fs;
use std::path::Path;

use forkspace::state::State;
use serial_test::serial;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
#[serial]
fn v1_migrates_atomically_with_backup_and_unknown_fields() {
    let home = common::setup_test_env();
    let source = fs::read(fixture("state-v1-with-unknown-fields.json")).unwrap();
    fs::write(home.path().join("state.json"), &source).unwrap();

    let state = State::load().unwrap();

    assert_eq!(state.version, 2);
    assert_eq!(state.unknown_fields["future_field"]["enabled"], true);
    assert_eq!(fs::read(home.path().join("state.v1.bak")).unwrap(), source);
    assert!(!home.path().join(".migrating").exists());
}

#[test]
#[serial]
fn interrupted_migration_restores_backup_before_retrying() {
    let home = common::setup_test_env();
    let source = fs::read(fixture("state-v1.json")).unwrap();
    fs::write(home.path().join("state.v1.bak"), &source).unwrap();
    fs::write(home.path().join("state.json"), b"partially written v2").unwrap();
    fs::write(home.path().join(".migrating"), b"1 -> 2\n").unwrap();

    let state = State::load().unwrap();

    assert_eq!(state.version, 2);
    assert!(state.workspaces.contains_key("fixture"));
    assert!(!home.path().join(".migrating").exists());
}

#[test]
#[serial]
fn v2_unknown_fields_survive_at_every_persisted_layer() {
    let home = common::setup_test_env();
    let json = serde_json::json!({
        "version": 2,
        "top_extra": "top",
        "workspaces": {
            "fixture": {
                "name": "fixture",
                "base": "/tmp/base",
                "parent": null,
                "created": "2025-01-01T00:00:00Z",
                "session": null,
                "ephemeral": false,
                "view_path": "/tmp/view",
                "fork_time_ns": 1,
                "backend_kind": "linux",
                "workspace_extra": "workspace",
                "backend_state": {
                    "kind": "linux",
                    "upper": "/tmp/upper",
                    "work": "/tmp/work",
                    "merged": "/tmp/view",
                    "tmpfs": false,
                    "tmpfs_size": null,
                    "backend_extra": "backend"
                },
                "process": {
                    "pid": 123,
                    "command": "test",
                    "port": 3000,
                    "process_extra": "process"
                }
            }
        },
        "proxy": {
            "listen_port": 4000,
            "active_workspace": "fixture",
            "target_port": 3000,
            "proxy_extra": "proxy"
        }
    });
    fs::write(
        home.path().join("state.json"),
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();

    State::load().unwrap().save().unwrap();

    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(saved["top_extra"], "top");
    assert_eq!(
        saved["workspaces"]["fixture"]["workspace_extra"],
        "workspace"
    );
    assert_eq!(
        saved["workspaces"]["fixture"]["backend_state"]["backend_extra"],
        "backend"
    );
    assert_eq!(
        saved["workspaces"]["fixture"]["process"]["process_extra"],
        "process"
    );
    assert_eq!(saved["proxy"]["proxy_extra"], "proxy");
}
