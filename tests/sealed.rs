//! Sealed overlay workspaces: the lower layer is a frozen copy of the base, so nothing done in
//! the base after the fork reaches a workspace until `pull`.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
}

impl Env {
    /// A Git project with `a.txt` and `b.txt` on `main` and a different `a.txt` on `other`.
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
        };
        let project = env.project();
        fs::create_dir(&project).unwrap();
        git(&project, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(project.join("a.txt"), "a\n").unwrap();
        fs::write(project.join("b.txt"), "b\n").unwrap();
        git(&project, &["add", "-A"]);
        git(&project, &["commit", "--quiet", "-m", "init"]);
        git(&project, &["switch", "--quiet", "-c", "other"]);
        fs::write(project.join("a.txt"), "other\n").unwrap();
        git(&project, &["commit", "--quiet", "-am", "other"]);
        git(&project, &["switch", "--quiet", "main"]);
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn view(&self, name: &str) -> PathBuf {
        self.home.path().join(name).join("merged")
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", "linux")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn new_workspace(&self, name: &str, extra: &[&str]) {
        let project = self.project();
        let mut args = vec!["new", name, "--from", project.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.run(&args);
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(args).stdout).unwrap()
    }

    fn frozen_copies(&self) -> Vec<PathBuf> {
        let root = self.home.path().join("frozen");
        let mut copies: Vec<_> = fs::read_dir(root)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.join("tree").is_dir())
                    .collect()
            })
            .unwrap_or_default();
        copies.sort();
        copies
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .args(["drop", "--all"])
            .output();
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Forkspace Tests",
            "-c",
            "user.email=t@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn branch_switches_and_commits_in_the_base_do_not_reach_the_workspace() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("ws", &[]);
    let view = env.view("ws");

    git(&env.project(), &["switch", "--quiet", "other"]);
    fs::write(env.project().join("b.txt"), "base edit\n").unwrap();
    git(&env.project(), &["commit", "--quiet", "-am", "base commit"]);
    git(&env.project(), &["branch", "base-only"]);

    assert_eq!(fs::read_to_string(view.join("a.txt")).unwrap(), "a\n");
    assert_eq!(fs::read_to_string(view.join("b.txt")).unwrap(), "b\n");
    assert_eq!(git(&view, &["branch", "--show-current"]).trim(), "main");
    assert_eq!(git(&view, &["status", "--porcelain"]), "");
    assert_eq!(git(&view, &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert!(!git(&view, &["branch", "--list"]).contains("base-only"));
    assert_eq!(env.stdout(&["diff", "ws"]).trim(), "clean — no changes");
}

#[test]
fn later_forks_reuse_the_frozen_copy_or_link_unchanged_files() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    env.new_workspace("two", &[]);
    assert_eq!(
        env.frozen_copies().len(),
        1,
        "an unchanged base reuses its copy"
    );

    fs::write(env.project().join("b.txt"), "changed\n").unwrap();
    env.new_workspace("three", &[]);
    let copies = env.frozen_copies();
    assert_eq!(copies.len(), 2);
    let inode = |copy: &Path, name: &str| fs::metadata(copy.join("tree").join(name)).unwrap().ino();
    assert_eq!(
        inode(&copies[0], "a.txt"),
        inode(&copies[1], "a.txt"),
        "unchanged file is linked"
    );
    assert_ne!(
        inode(&copies[0], "b.txt"),
        inode(&copies[1], "b.txt"),
        "changed file is copied"
    );
    assert_eq!(
        fs::read_to_string(env.view("three").join("b.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        fs::read_to_string(env.view("one").join("b.txt")).unwrap(),
        "b\n"
    );
}

#[test]
fn forks_of_one_frozen_copy_share_its_manifest() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    let copies = env.frozen_copies();
    let cache = copies[0].join("manifest.jsonl");
    assert!(
        cache.is_file(),
        "the first fork keeps its manifest for the next"
    );

    env.new_workspace("two", &[]);
    let manifests = env.home.path().join("manifests");
    let two = manifests.join("two.jsonl");
    assert_eq!(
        fs::read(manifests.join("one.jsonl")).unwrap(),
        fs::read(&two).unwrap()
    );
    let cached = fs::read(&cache).unwrap();

    // The copied manifest is a working baseline: an edit shows in diff and merges back.
    fs::write(env.view("two").join("a.txt"), "edited\n").unwrap();
    assert!(env
        .stdout(&["diff", "two", "--stat"])
        .contains("1 modified"));
    env.run(&["merge", "two", "--no-install"]);
    assert_eq!(
        fs::read_to_string(env.project().join("a.txt")).unwrap(),
        "edited\n"
    );

    assert_eq!(
        fs::read(&cache).unwrap(),
        cached,
        "work in a workspace never changes the copy's manifest"
    );

    // A changed project gets a new frozen copy, which records its own manifest.
    env.new_workspace("three", &[]);
    let copies = env.frozen_copies();
    assert_eq!(copies.len(), 2);
    assert!(copies
        .iter()
        .all(|copy| copy.join("manifest.jsonl").is_file()));
}

/// A successful fuse-overlayfs probe is remembered, and a record that no longer matches the
/// machine is replaced by a fresh probe.
#[test]
fn a_successful_fuse_probe_is_remembered() {
    skip_without_overlay!();
    let env = Env::new();
    let project = env.project();
    let fork = |name: &str| {
        // No FORKSPACE_BACKEND: the backend is chosen by probing.
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", env.home.path())
            .args(["new", name, "--from", project.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let record = env.home.path().join("fuse-overlayfs-probe");

    fork("one");
    let saved = fs::read_to_string(&record).unwrap();
    assert!(
        saved.contains("binary=") && saved.contains("boot="),
        "{saved}"
    );

    fs::write(&record, "binary=/nowhere\n").unwrap();
    fork("two");
    assert_eq!(fs::read_to_string(&record).unwrap(), saved);
    assert_eq!(env.stdout(&["diff", "two"]).trim(), "clean — no changes");
}

/// Wait for the background flush of `copy` to mark it durable.
fn wait_for_durable(copy: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !copy.join("durable").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} was never marked durable",
            copy.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Pretend the frozen copy `copy` was written in an earlier boot and never flushed.
fn unflush_from_earlier_boot(copy: &Path) {
    wait_for_durable(copy);
    fs::remove_file(copy.join("durable")).unwrap();
    let meta = fs::read_to_string(copy.join("meta.json")).unwrap();
    let mut meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    meta["boot"] = serde_json::Value::String("an-earlier-boot".to_string());
    fs::write(copy.join("meta.json"), meta.to_string()).unwrap();
}

#[test]
fn frozen_copies_are_flushed_in_the_background() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    let copies = env.frozen_copies();
    wait_for_durable(&copies[0]);
    assert!(fs::read_to_string(copies[0].join("meta.json"))
        .unwrap()
        .contains("\"boot\""));
}

#[test]
fn an_intact_copy_left_unflushed_by_a_crash_is_verified_and_kept() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    let copy = env.frozen_copies().remove(0);
    unflush_from_earlier_boot(&copy);

    let listed = env.run(&["ls"]);
    assert!(String::from_utf8_lossy(&listed.stderr).is_empty());
    assert!(
        copy.join("durable").exists(),
        "an intact copy is marked durable"
    );
    env.new_workspace("two", &[]);
    assert_eq!(env.frozen_copies(), [copy], "the verified copy is reused");
}

#[test]
fn a_damaged_copy_left_unflushed_by_a_crash_is_reported_and_never_reused() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    let copy = env.frozen_copies().remove(0);
    unflush_from_earlier_boot(&copy);
    // What a crash before the flush leaves: a file whose data never reached the disk.
    let torn = copy.join("tree/b.txt");
    let mut permissions = fs::metadata(&torn).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o644);
    fs::set_permissions(&torn, permissions).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&torn)
        .unwrap();

    let listed = env.run(&["ls"]);
    let warning = String::from_utf8_lossy(&listed.stderr);
    assert!(
        warning.contains("damaged") && warning.contains("one"),
        "{warning}"
    );
    let again = env.run(&["ls"]);
    assert!(
        String::from_utf8_lossy(&again.stderr).is_empty(),
        "reported once"
    );

    env.new_workspace("two", &[]);
    let copies = env.frozen_copies();
    assert_eq!(copies.len(), 2, "the damaged copy is not reused");
    assert_eq!(
        fs::read_to_string(env.view("two").join("b.txt")).unwrap(),
        "b\n"
    );
}

/// Every path under `root` with its kind and content, for comparing two trees.
fn snapshot_tree(root: &Path) -> Vec<(String, String)> {
    let mut listed = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            if relative == ".git" || relative.starts_with(".git/") {
                continue;
            }
            let kind = entry.file_type().unwrap();
            let what = if kind.is_symlink() {
                format!("link {}", fs::read_link(&path).unwrap().display())
            } else if kind.is_dir() {
                pending.push(path);
                "dir".to_string()
            } else {
                format!("file {}", fs::read_to_string(&path).unwrap())
            };
            listed.push((relative, what));
        }
    }
    listed.sort();
    listed
}

#[test]
fn a_stacked_copy_matches_its_parent_view_after_every_kind_of_change() {
    skip_without_overlay!();
    let env = Env::new();
    let project = env.project();
    for directory in ["keep/inner", "gone/deep", "reborn", "swap"] {
        fs::create_dir_all(project.join(directory)).unwrap();
    }
    fs::write(project.join("keep/inner/k.txt"), "k\n").unwrap();
    fs::write(project.join("gone/deep/g.txt"), "g\n").unwrap();
    fs::write(project.join("reborn/old.txt"), "old\n").unwrap();
    fs::write(project.join("swap/s.txt"), "s\n").unwrap();
    std::os::unix::fs::symlink("a.txt", project.join("link")).unwrap();
    env.new_workspace("parent", &[]);

    let view = env.view("parent");
    fs::write(view.join("a.txt"), "edited\n").unwrap();
    fs::remove_file(view.join("b.txt")).unwrap();
    fs::write(view.join("added.txt"), "added\n").unwrap();
    fs::remove_dir_all(view.join("gone")).unwrap();
    fs::remove_dir_all(view.join("reborn")).unwrap();
    fs::create_dir(view.join("reborn")).unwrap();
    fs::write(view.join("reborn/new.txt"), "new\n").unwrap();
    fs::remove_dir_all(view.join("swap")).unwrap();
    fs::write(view.join("swap"), "now a file\n").unwrap();
    fs::create_dir_all(view.join("fresh/nested")).unwrap();
    fs::write(view.join("fresh/nested/f.txt"), "f\n").unwrap();
    fs::write(view.join("keep/inner/k2.txt"), "k2\n").unwrap();

    let parent = view.to_str().unwrap().to_string();
    let _ = parent;
    env.run(&["new", "child", "--from", "parent"]);
    assert_eq!(snapshot_tree(&env.view("child")), snapshot_tree(&view));
    assert_eq!(env.stdout(&["diff", "child"]).trim(), "clean — no changes");
}

#[test]
fn merge_then_pull_moves_one_workspace_s_changes_into_another() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("ws1", &[]);
    env.new_workspace("ws2", &[]);

    fs::write(env.view("ws1").join("feature.txt"), "from ws1\n").unwrap();
    env.run(&["merge", "ws1", "--quiet", "--no-install"]);
    assert!(
        !env.view("ws2").join("feature.txt").exists(),
        "updates wait for pull"
    );

    fs::write(env.view("ws2").join("own.txt"), "ws2\n").unwrap();
    env.run(&["pull", "ws2", "--no-install"]);
    assert_eq!(
        fs::read_to_string(env.view("ws2").join("feature.txt")).unwrap(),
        "from ws1\n"
    );
    assert_eq!(env.stdout(&["diff", "ws2", "--files"]).trim(), "own.txt");
}

#[test]
fn a_stacked_workspace_is_sealed_against_its_parent() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("parent", &[]);
    fs::write(env.view("parent").join("before.txt"), "before\n").unwrap();
    env.run(&["new", "child", "--from", "parent"]);
    fs::write(env.view("parent").join("after.txt"), "after\n").unwrap();

    assert!(env.view("child").join("before.txt").is_file());
    assert!(!env.view("child").join("after.txt").exists());
    assert_eq!(env.stdout(&["diff", "child"]).trim(), "clean — no changes");
}

#[test]
fn drop_removes_frozen_copies_no_workspace_uses() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    fs::write(env.project().join("b.txt"), "changed\n").unwrap();
    env.new_workspace("two", &[]);
    assert_eq!(env.frozen_copies().len(), 2);

    env.run(&["drop", "one"]);
    assert_eq!(env.frozen_copies().len(), 1);
    env.run(&["drop", "two"]);
    assert!(env.frozen_copies().is_empty());
}

#[test]
fn live_workspaces_mount_the_base_itself() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("live", &["--live"]);
    // What a live view shows after the base changes is undefined (fuse-overlayfs caches
    // what it has loaded), which is why workspaces are sealed by default; only check that
    // no frozen copy is made and the workspace is labelled.
    assert!(env.frozen_copies().is_empty());
    assert_eq!(
        fs::read_to_string(env.view("live").join("a.txt")).unwrap(),
        "a\n"
    );
    assert!(env.stdout(&["ls"]).contains("linux (live)"));
    let listed: serde_json::Value = serde_json::from_str(&env.stdout(&["ls", "--json"])).unwrap();
    assert_eq!(listed[0]["live"], true);
}
