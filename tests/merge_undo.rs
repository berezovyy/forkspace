//! `merge --undo`: a merge keeps what it replaces or deletes in the project.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(env.project()).unwrap();
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn view(&self, name: &str) -> PathBuf {
        self.home.path().join("workspaces").join(name)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", "copy")
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn fork(&self, name: &str) {
        let project = self.project();
        self.ok(&["new", name, "--from", project.to_str().unwrap(), "--copy"]);
    }

    fn merge(&self, name: &str) {
        self.ok(&["merge", name, "--quiet", "--no-install"]);
    }

    /// Undo records (directories; the store also holds its backup exclusion tag).
    fn undo_dirs(&self) -> usize {
        fs::read_dir(self.home.path().join("undo"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .count()
            })
            .unwrap_or(0)
    }
}

/// Content, permissions, and modification time of every entry under `root`.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, u32, i64)> {
    let mut entries: Vec<_> = walk(root)
        .into_iter()
        .map(|path| {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let bytes = if metadata.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            let mtime = if metadata.is_file() {
                metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000
            } else {
                0
            };
            (
                path.strip_prefix(root).unwrap().to_path_buf(),
                bytes,
                metadata.mode(),
                mtime,
            )
        })
        .collect();
    entries.sort();
    entries
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(root).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            paths.extend(walk(&path));
        }
        paths.push(path);
    }
    paths
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn undo_restores_the_exact_project_and_removes_what_merge_added() {
    let env = Env::new();
    fs::write(env.project().join("app.txt"), "original\n").unwrap();
    fs::write(env.project().join("notes.txt"), "untracked notes\n").unwrap();
    fs::set_permissions(
        env.project().join("notes.txt"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    env.fork("ws");
    let before = snapshot(&env.project());

    fs::write(env.view("ws").join("app.txt"), "agent rewrite\n").unwrap();
    fs::remove_file(env.view("ws").join("notes.txt")).unwrap();
    fs::create_dir_all(env.view("ws").join("new/dir")).unwrap();
    fs::write(env.view("ws").join("new/dir/added.txt"), "added\n").unwrap();
    env.merge("ws");
    assert!(!env.project().join("notes.txt").exists());

    let listed = String::from_utf8(env.ok(&["merge", "--undo", "--list"]).stdout).unwrap();
    assert!(listed.contains("  ws  "), "{listed}");
    let undone = String::from_utf8(env.ok(&["merge", "--undo", "--no-install"]).stdout).unwrap();
    assert!(undone.contains("undid the merge of 'ws'"), "{undone}");
    assert_eq!(snapshot(&env.project()), before);
    assert_eq!(
        env.undo_dirs(),
        0,
        "a fully undone merge leaves no undo data"
    );
}

#[test]
fn files_edited_after_the_merge_are_left_alone_and_kept() {
    let env = Env::new();
    fs::write(env.project().join("a.txt"), "a0\n").unwrap();
    fs::write(env.project().join("b.txt"), "b0\n").unwrap();
    env.fork("ws");
    fs::write(env.view("ws").join("a.txt"), "a1\n").unwrap();
    fs::write(env.view("ws").join("b.txt"), "b1\n").unwrap();
    env.merge("ws");
    fs::write(
        env.project().join("b.txt"),
        "edited by you after the merge\n",
    )
    .unwrap();

    let output = env.ok(&["merge", "--undo", "--no-install"]);
    assert!(
        stderr(&output).contains("changed after the merge"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(env.project().join("a.txt")).unwrap(),
        "a0\n"
    );
    assert_eq!(
        fs::read_to_string(env.project().join("b.txt")).unwrap(),
        "edited by you after the merge\n"
    );
    assert_eq!(env.undo_dirs(), 1, "the kept version of b.txt stays");
}

#[test]
fn undoing_an_older_merge_refuses_files_a_newer_merge_changed() {
    let env = Env::new();
    fs::write(env.project().join("shared.txt"), "0\n").unwrap();
    fs::write(env.project().join("own.txt"), "0\n").unwrap();
    env.fork("ws1");
    env.fork("ws2");
    fs::write(env.view("ws1").join("shared.txt"), "1\n").unwrap();
    fs::write(env.view("ws1").join("own.txt"), "1\n").unwrap();
    env.merge("ws1");
    env.ok(&["pull", "ws2", "--no-install"]);
    fs::write(env.view("ws2").join("shared.txt"), "2\n").unwrap();
    env.merge("ws2");

    let output = env.ok(&["merge", "--undo", "ws1", "--no-install"]);
    assert!(
        stderr(&output).contains("newer merge"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(env.project().join("own.txt")).unwrap(),
        "0\n"
    );
    assert_eq!(
        fs::read_to_string(env.project().join("shared.txt")).unwrap(),
        "2\n"
    );

    env.ok(&["merge", "--undo", "ws2", "--no-install"]);
    assert_eq!(
        fs::read_to_string(env.project().join("shared.txt")).unwrap(),
        "1\n"
    );
    env.ok(&["merge", "--undo", "ws1", "--no-install"]);
    assert_eq!(
        fs::read_to_string(env.project().join("shared.txt")).unwrap(),
        "0\n"
    );
}

#[test]
fn a_resumed_merge_can_still_be_undone() {
    let env = Env::new();
    fs::write(env.project().join("a.txt"), "a0\n").unwrap();
    fs::create_dir(env.project().join("z")).unwrap();
    fs::write(env.project().join("z/b.txt"), "b0\n").unwrap();
    env.fork("ws");
    let before = snapshot(&env.project());
    fs::write(env.view("ws").join("a.txt"), "a1\n").unwrap();
    fs::write(env.view("ws").join("z/b.txt"), "b1\n").unwrap();

    // The second step cannot write into a read-only directory: the merge stops half way.
    let locked = env.project().join("z");
    let mode = fs::metadata(&locked).unwrap().permissions().mode();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = env.run(&["merge", "ws", "--quiet", "--no-install"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(mode)).unwrap();
    assert!(!failed.status.success());
    assert_eq!(
        fs::read_to_string(env.project().join("a.txt")).unwrap(),
        "a1\n"
    );

    env.ok(&["merge", "ws", "--resume", "--no-install"]);
    assert_eq!(
        fs::read_to_string(env.project().join("z/b.txt")).unwrap(),
        "b1\n"
    );
    env.ok(&["merge", "--undo", "--no-install"]);
    assert_eq!(snapshot(&env.project()), before);
}

#[test]
fn undo_data_beyond_ten_merges_and_two_weeks_is_removed() {
    let env = Env::new();
    fs::write(env.project().join("a.txt"), "0\n").unwrap();
    let undo_root = env.home.path().join("undo");
    fs::create_dir_all(&undo_root).unwrap();
    let month_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        - 30 * 24 * 3600 * 1_000_000_000;
    for index in 0..12_u128 {
        let created_ns = month_ago + index;
        let dir = undo_root.join(format!("{created_ns:020}-old"));
        fs::create_dir_all(dir.join("kept")).unwrap();
        let record = serde_json::json!({
            "workspace": "old",
            "project": env.project(),
            "merged_at": "2026-09-13T00:00:00Z",
            "created_ns": created_ns,
            "complete": true,
            "steps": [],
        });
        fs::write(dir.join("record.json"), record.to_string()).unwrap();
    }
    env.fork("ws");
    fs::write(env.view("ws").join("a.txt"), "1\n").unwrap();
    env.merge("ws");
    assert_eq!(env.undo_dirs(), 10);
    let listed = String::from_utf8(env.ok(&["merge", "--undo", "--list"]).stdout).unwrap();
    assert!(
        listed.lines().next().unwrap().contains("  ws  "),
        "newest first: {listed}"
    );
}
