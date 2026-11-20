//! `forkspace hook`: Claude Code's WorktreeCreate and WorktreeRemove hooks, and
//! `forkspace skill`.

mod common;

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use common::cli::{stderr, stdout, write, Env};
use serial_test::serial;

fn hook(env: &Env, args: &[&str], input: &serde_json::Value) -> Output {
    let mut child = env
        .command(&[&["hook"], args].concat())
        .env_remove("CLAUDE_PROJECT_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn create(env: &Env, name: &str) -> PathBuf {
    let input = serde_json::json!({
        "session_id": "abc123",
        "cwd": env.project().join("src"),
        "hook_event_name": "WorktreeCreate",
        "name": name,
    });
    let output = hook(env, &["worktree-create"], &input);
    assert!(output.status.success(), "{}", stderr(&output));
    let printed = stdout(&output);
    let last = printed
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap();
    PathBuf::from(last)
}

fn remove(env: &Env, path: &std::path::Path, args: &[&str]) -> Output {
    let input = serde_json::json!({
        "session_id": "abc123",
        "cwd": env.project(),
        "hook_event_name": "WorktreeRemove",
        "worktree_path": path,
    });
    hook(env, &[&["worktree-remove"], args].concat(), &input)
}

fn project(env: &Env) {
    write(&env.project().join("src/main.rs"), "fn main() {}\n");
    write(&env.project().join(".env"), "SECRET=1\n");
    write(&env.project().join(".gitignore"), ".env\n");
    git(&env.project(), &["init", "-q"]);
    git(&env.project(), &["add", "-A"]);
    git(&env.project(), &["commit", "-qm", "init"]);
}

#[test]
#[serial]
fn worktree_create_forks_the_repository_and_prints_only_the_path() {
    let env = Env::new();
    project(&env);
    let path = create(&env, "bold-oak-a3f2");
    assert!(path.is_absolute());
    assert!(path.join("src/main.rs").exists());
    assert!(path.join(".env").exists(), "ignored files come along");
    let workspace = env.workspace("bold-oak-a3f2");
    assert_eq!(
        workspace.view_path.canonicalize().unwrap(),
        path.canonicalize().unwrap()
    );
    assert_eq!(workspace.branch.as_deref(), Some("worktree-bold-oak-a3f2"));
    // The same name opens the same workspace again.
    assert_eq!(create(&env, "bold-oak-a3f2"), path);
}

#[test]
#[serial]
fn worktree_remove_drops_a_clean_workspace_and_keeps_one_with_work() {
    let env = Env::new();
    project(&env);
    let clean = create(&env, "clean");
    let output = remove(&env, &clean, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!clean.exists());

    let busy = create(&env, "busy");
    write(&busy.join("src/new.rs"), "// work\n");
    let output = remove(&env, &busy, &[]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("kept workspace 'busy'"),
        "{}",
        stderr(&output)
    );
    assert!(busy.join("src/new.rs").exists());

    let output = remove(&env, &busy, &["--discard"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!busy.exists());
}

#[test]
#[serial]
fn claude_settings_and_skill_print_what_they_document() {
    let env = Env::new();
    let settings: serde_json::Value =
        serde_json::from_slice(&env.ok(&["hook", "claude-settings"]).stdout).unwrap();
    assert_eq!(
        settings["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
        "forkspace hook worktree-create"
    );
    assert_eq!(
        settings["hooks"]["WorktreeRemove"][0]["hooks"][0]["type"],
        "command"
    );
    let skill = stdout(&env.ok(&["skill"]));
    assert!(skill.starts_with("---\nname: forkspace\n"));
    // Every command the skill names exists.
    for command in [
        "new", "enter", "diff", "merge", "undo", "pull", "save", "restore", "drop",
    ] {
        assert!(skill.contains(&format!("forkspace {command}")), "{command}");
        let help = env.run(&[command, "--help"]);
        assert!(help.status.success(), "{command}");
    }
}
