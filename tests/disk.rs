//! Disk use in `ls`, `forkspace gc`, and keeping internal stores out of backups.

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

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
        fs::write(env.project().join("app.txt"), "app\n").unwrap();
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn run(&self, args: &[&str], backend: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", backend)
            .env("FORKSPACE_NO_SYSTEMD", "1")
            .env("NO_COLOR", "1")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str], backend: &str) -> String {
        let output = self.run(args, backend);
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fork(&self, name: &str, backend: &str) {
        let project = self.project();
        let mut args = vec!["new", name, "--from", project.to_str().unwrap()];
        if backend == "copy" {
            args.push("--copy");
        }
        self.ok(&args, backend);
    }

    fn sizes(&self) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["ls", "--json"], "copy")).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["drop", "--all", "--kill"], "copy");
    }
}

fn size_of(listed: &serde_json::Value, name: &str) -> u64 {
    listed
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap()["size_bytes"]
        .as_u64()
        .unwrap()
}

#[test]
fn ls_shows_private_sizes_and_a_total() {
    let env = Env::new();
    env.fork("copied", "copy");
    fs::write(
        env.home.path().join("workspaces/copied/big.bin"),
        vec![7_u8; 256 * 1024],
    )
    .unwrap();
    assert!(size_of(&env.sizes(), "copied") >= 256 * 1024);
    let table = env.ok(&["ls"], "copy");
    assert!(table.contains("SIZE"), "{table}");
    assert!(table.contains("forkspace uses"), "{table}");
}

#[test]
fn a_fresh_overlay_workspace_costs_almost_nothing() {
    skip_without_overlay!();
    let env = Env::new();
    env.fork("fresh", "linux");
    assert!(size_of(&env.sizes(), "fresh") < 64 * 1024);
}

#[test]
fn gc_lists_idle_workspaces_and_expired_data_but_removes_nothing_unasked() {
    let env = Env::new();
    env.fork("idle", "copy");
    env.fork("busy", "copy");
    fs::write(env.home.path().join("workspaces/busy/work.txt"), "work\n").unwrap();
    // Undo data for twelve merges a month ago: two are past retention (10 merges or 14 days).
    let month_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        - 30 * 24 * 3600 * 1_000_000_000;
    for index in 0..12_u128 {
        let created_ns = month_ago + index;
        let dir = env.home.path().join(format!("undo/{created_ns:020}-old"));
        fs::create_dir_all(dir.join("kept")).unwrap();
        let record = serde_json::json!({
            "workspace": "old", "project": env.project(), "merged_at": "2026-09-13T00:00:00Z",
            "created_ns": created_ns, "complete": true, "steps": [],
        });
        fs::write(dir.join("record.json"), record.to_string()).unwrap();
    }
    // An unused frozen copy is removed by crash recovery on the next command already.
    let unused = env.home.path().join("frozen/deadbeef-1/tree");
    fs::create_dir_all(&unused).unwrap();

    let fresh = env.ok(&["gc"], "copy");
    assert!(
        !fresh.contains("workspace 'idle'"),
        "younger than 7 days: {fresh}"
    );
    assert!(!unused.exists(), "recovery sweeps unused frozen copies");

    let survey = env.ok(&["gc", "--older-than", "0s"], "copy");
    assert!(survey.contains("workspace 'idle'"), "{survey}");
    assert_eq!(survey.matches("undo data").count(), 2, "{survey}");
    assert!(survey.contains("keeping 'busy'"), "{survey}");
    assert!(!survey.contains("workspace 'busy'"), "{survey}");
    assert!(survey.contains("nothing removed"), "{survey}");
    assert!(env.home.path().join("workspaces/idle").exists());

    let removed = env.ok(&["gc", "--older-than", "0s", "--yes"], "copy");
    assert!(removed.contains("freed"), "{removed}");
    assert!(!env.home.path().join("workspaces/idle").exists());
    assert!(env.home.path().join("workspaces/busy").exists());
    assert_eq!(
        fs::read_dir(env.home.path().join("undo")).unwrap().count(),
        10
    );
}

#[test]
fn every_internal_store_is_tagged_for_backups() {
    let env = Env::new();
    env.fork("ws", "copy");
    fs::write(env.home.path().join("workspaces/ws/app.txt"), "changed\n").unwrap();
    env.ok(&["checkpoint", "ws"], "copy");
    env.ok(&["merge", "ws", "--quiet", "--no-install"], "copy");
    env.ok(&["drop", "ws"], "copy");
    env.ok(&["ls"], "copy");
    for store in ["undo", "checkpoints", "ancestors", "trash"] {
        let tag = env.home.path().join(store).join("CACHEDIR.TAG");
        let text = fs::read_to_string(&tag).unwrap_or_else(|_| panic!("{} missing", tag.display()));
        assert!(text.starts_with("Signature: 8a477f597d28d172789f06886806bc55"));
    }
}
