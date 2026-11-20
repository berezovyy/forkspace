//! `forkspace setup` gives Claude Code the forkspace skill and worktree hooks, keeps what its
//! settings already hold, and changes nothing when run again (`src/commands/setup.rs`).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn setup(home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("HOME", home)
        .env("FORKSPACE_HOME", home.join("forkspace"))
        .arg("setup")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn setup_installs_the_skill_and_hooks_once_and_keeps_other_settings() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(
        claude.join("settings.json"),
        r#"{"model": "opus", "hooks": {"PostToolUse": [{"hooks": [{"type": "command", "command": "mine"}]}]}}"#,
    )
    .unwrap();

    let first = setup(home.path());
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        stdout(&first).contains("added the forkspace skill and the worktree hooks"),
        "{}",
        stdout(&first)
    );
    let skill = fs::read_to_string(claude.join("skills/forkspace/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: forkspace\n"));
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["model"], "opus");
    assert_eq!(
        settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "mine"
    );
    assert_eq!(
        settings["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
        "forkspace hook worktree-create"
    );
    assert_eq!(
        settings["hooks"]["WorktreeRemove"][0]["hooks"][0]["command"],
        "forkspace hook worktree-remove"
    );

    let again = setup(home.path());
    assert!(again.status.success());
    assert!(
        stdout(&again).contains("already set up"),
        "{}",
        stdout(&again)
    );
    let settings_again: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings_again, settings, "a second run adds nothing");
}

#[test]
fn setup_without_a_coding_agent_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let output = setup(home.path());
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("no coding agent found"),
        "{}",
        stdout(&output)
    );
    assert!(!home.path().join(".claude").exists());
}
