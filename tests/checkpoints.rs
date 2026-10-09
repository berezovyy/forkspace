//! Checkpoints: save a workspace's state and restore it later.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    backend: &'static str,
}

impl Env {
    fn new(backend: &'static str) -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
            backend,
        };
        let project = env.root.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("keep.txt"), "keep\n").unwrap();
        fs::write(project.join("edit.txt"), "fork\n").unwrap();
        fs::write(project.join("gone.txt"), "gone\n").unwrap();
        fs::write(project.join("later.txt"), "fork version\n").unwrap();
        let mut args = vec!["new", "ws", "--from", project.to_str().unwrap()];
        if backend == "copy" {
            args.push("--copy");
        }
        env.ok(&args);
        env
    }

    fn view(&self) -> PathBuf {
        if self.backend == "linux" {
            self.home.path().join("ws/merged")
        } else {
            self.home.path().join("workspaces/ws")
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", self.backend)
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["drop", "--all"]);
    }
}

/// Content, mode, and mtime of every visible file in the view (skipping `.git`).
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, u32, i64, i64)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                stack.push(path);
            } else {
                files.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                    metadata.mode(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                ));
            }
        }
    }
    files.sort();
    files
}

fn checkpoint_then_restore_is_exact(backend: &'static str) {
    let env = Env::new(backend);
    let view = env.view();
    fs::write(view.join("edit.txt"), "checkpointed\n").unwrap();
    fs::set_permissions(view.join("edit.txt"), fs::Permissions::from_mode(0o750)).unwrap();
    fs::remove_file(view.join("gone.txt")).unwrap();
    fs::create_dir(view.join("src")).unwrap();
    fs::write(view.join("src/new.txt"), "new\n").unwrap();
    let saved = env.ok(&["checkpoint", "ws", "-m", "good state"]);
    assert!(saved.contains("saved checkpoint 1"), "{saved}");
    let expected = snapshot(&view);

    // A bad step: rewrite, delete, add, and touch a file the checkpoint did not change.
    fs::write(view.join("edit.txt"), "broken\n").unwrap();
    fs::remove_file(view.join("src/new.txt")).unwrap();
    fs::write(view.join("gone.txt"), "back from the dead\n").unwrap();
    fs::write(view.join("junk.txt"), "junk\n").unwrap();
    fs::write(view.join("later.txt"), "changed after the checkpoint\n").unwrap();

    let restored = env.ok(&["restore", "ws", "1"]);
    assert!(
        restored.contains("restored 'ws' to checkpoint 1"),
        "{restored}"
    );
    assert_eq!(snapshot(&view), expected);
    assert_eq!(
        fs::read_to_string(view.join("later.txt")).unwrap(),
        "fork version\n"
    );

    let listed = env.ok(&["checkpoints", "ws"]);
    assert!(listed.contains("good state"), "{listed}");
    assert!(listed.contains("(auto) before restore to 1"), "{listed}");
}

#[test]
fn checkpoint_then_restore_is_exact_on_the_copy_backend() {
    checkpoint_then_restore_is_exact("copy");
}

#[test]
fn checkpoint_then_restore_is_exact_on_a_sealed_overlay() {
    skip_without_overlay!();
    checkpoint_then_restore_is_exact("linux");
}

#[test]
fn the_automatic_checkpoint_undoes_a_restore() {
    let env = Env::new("copy");
    let view = env.view();
    fs::write(view.join("edit.txt"), "one\n").unwrap();
    env.ok(&["checkpoint", "ws"]);
    fs::write(view.join("edit.txt"), "two\n").unwrap();
    let before = snapshot(&view);
    let restored = env.ok(&["restore", "ws", "1"]);
    assert_eq!(fs::read_to_string(view.join("edit.txt")).unwrap(), "one\n");
    assert!(
        restored.contains("undo with `forkspace restore ws 2`"),
        "{restored}"
    );
    env.ok(&["restore", "ws", "2"]);
    assert_eq!(snapshot(&view), before);
}

#[test]
fn an_interrupted_restore_resumes() {
    let env = Env::new("copy");
    let view = env.view();
    fs::write(view.join("edit.txt"), "saved\n").unwrap();
    fs::create_dir(view.join("z")).unwrap();
    fs::write(view.join("z/inner.txt"), "saved\n").unwrap();
    env.ok(&["checkpoint", "ws"]);
    let expected = snapshot(&view);
    fs::write(view.join("edit.txt"), "later\n").unwrap();
    fs::write(view.join("z/inner.txt"), "later\n").unwrap();

    let locked = view.join("z");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = env.run(&["restore", "ws", "1"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!failed.status.success());
    assert_eq!(
        fs::read_to_string(view.join("edit.txt")).unwrap(),
        "saved\n"
    );

    env.ok(&["restore", "ws", "--resume"]);
    assert_eq!(snapshot(&view), expected);
}

#[test]
fn pull_takes_an_automatic_checkpoint_and_drop_removes_them_all() {
    let env = Env::new("copy");
    fs::write(env.view().join("edit.txt"), "ours\n").unwrap();
    fs::write(env.root.path().join("project/keep.txt"), "base update\n").unwrap();
    env.ok(&["pull", "ws", "--no-install"]);
    let listed = env.ok(&["checkpoints", "ws"]);
    assert!(listed.contains("(auto) before pull"), "{listed}");
    assert!(env.home.path().join("checkpoints/ws").is_dir());
    env.ok(&["drop", "ws"]);
    assert!(!env.home.path().join("checkpoints/ws").exists());
}

#[test]
fn retention_keeps_twenty_manual_checkpoints() {
    let env = Env::new("copy");
    for round in 0..22 {
        fs::write(env.view().join("edit.txt"), format!("{round}\n")).unwrap();
        env.ok(&["checkpoint", "ws"]);
    }
    let listed: serde_json::Value =
        serde_json::from_str(&env.ok(&["checkpoints", "ws", "--json"])).unwrap();
    let ids: Vec<u64> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|checkpoint| checkpoint["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, (3..=22).collect::<Vec<_>>());
}
