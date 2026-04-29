mod common;

use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use forkspace::backend::{select_backend, BackendKind};
use forkspace::error::ForkspaceError;
use forkspace::state::State;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn forkspace(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home)
        .args(args)
        .output()
        .unwrap()
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
#[serial]
fn reconciliation_sweeps_only_terminal_journals_and_is_idempotent() {
    let home = common::setup_test_env();
    let journal = home.path().join("journal");
    fs::create_dir_all(&journal).unwrap();
    let merge = journal.join("merge-work-1.json");
    fs::write(
        &merge,
        br#"{"op":"merge","workspace":"work","changes":[{}],"next":0}"#,
    )
    .unwrap();
    assert!(matches!(
        forkspace::reconcile::guard_merge("work"),
        Err(ForkspaceError::InterruptedOperation { .. })
    ));
    State::load().unwrap();
    assert!(
        merge.exists(),
        "non-terminal operations are never auto-repaired"
    );

    fs::write(
        &merge,
        br#"{"op":"merge","workspace":"work","changes":[{}],"next":1}"#,
    )
    .unwrap();
    let pull = journal.join("pull-work-1.json");
    fs::write(&pull, br#"{"op":"pull","workspace":"work","phase":"done"}"#).unwrap();
    State::load().unwrap();
    assert!(!merge.exists());
    assert!(!pull.exists());
    State::load().unwrap();
    assert!(!merge.exists());

    let orphan = home.path().join("manifests/orphan.jsonl");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, "{}\n").unwrap();
    State::load().unwrap();
    assert!(!orphan.exists());
}

#[test]
#[serial]
fn ten_parallel_state_transactions_do_not_lose_workspaces() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(10));
    let mut workers = Vec::new();
    for index in 0..10 {
        let barrier = barrier.clone();
        let base = base.path().to_path_buf();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let name = format!("parallel-{index}");
            State::with_state(|state| state.add_workspace(Workspace::new(&name, base, None)))
                .unwrap();
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    let state = State::load().unwrap();
    assert_eq!(
        state
            .workspaces
            .keys()
            .filter(|name| name.starts_with("parallel-"))
            .count(),
        10
    );
}

#[test]
#[serial]
fn ten_parallel_public_forks_commit_without_state_or_tree_corruption() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    common::create_test_project(base.path());
    let barrier = Arc::new(Barrier::new(10));
    let mut workers = Vec::new();
    for index in 0..10 {
        let barrier = barrier.clone();
        let home = home.path().to_path_buf();
        let base = base.path().to_path_buf();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            Command::new(env!("CARGO_BIN_EXE_forkspace"))
                .env("FORKSPACE_HOME", home)
                .args([
                    "fork",
                    base.to_str().unwrap(),
                    "--name",
                    &format!("public-{index}"),
                    "--copy",
                ])
                .output()
                .unwrap()
        }));
    }
    for output in workers.into_iter().map(|worker| worker.join().unwrap()) {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let state = State::load().unwrap();
    assert_eq!(state.workspaces.len(), 10);
    for index in 0..10 {
        let workspace = state.require_workspace(&format!("public-{index}")).unwrap();
        assert_eq!(workspace.backend_kind, BackendKind::PlainCopy);
        assert_eq!(
            fs::read(workspace.view_path.join("README.md")).unwrap(),
            b"# Test Project\n"
        );
    }
}

#[test]
#[serial]
fn kill_during_copy_fork_reconciles_the_intent_and_partial_tree() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    for index in 0..4_000 {
        let directory = base.path().join(format!("tree/{:03}", index / 100));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(format!("{index:05}")), index.to_string()).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args([
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "interrupted",
            "--copy",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let destination = home.path().join("workspaces/interrupted");
    assert!(wait_until(Duration::from_secs(10), || {
        let intent = fs::read(home.path().join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|state| state["pending_creates"].as_array().cloned())
            .is_some_and(|pending| !pending.is_empty());
        intent && destination.is_dir()
    }));
    child.kill().unwrap();
    let _ = child.wait();

    let listed = forkspace(home.path(), &["ls", "--json"]);
    assert!(listed.status.success());
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("interrupted"));
    assert!(!destination.exists());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("state.json")).unwrap()).unwrap();
    assert!(state["pending_creates"]
        .as_array()
        .is_none_or(Vec::is_empty));
}

#[test]
#[serial]
fn kill_during_drop_is_inferred_from_trash_and_is_idempotent() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    for index in 0..4_000 {
        let directory = base.path().join(format!("tree/{:03}", index / 100));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(format!("{index:05}")), b"x").unwrap();
    }
    assert!(forkspace(
        home.path(),
        &[
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "drop-kill",
            "--copy"
        ]
    )
    .status
    .success());
    let view = home.path().join("workspaces/drop-kill");
    let mut child = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args(["drop", "drop-kill"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_until(Duration::from_secs(10), || !view.exists()));
    let _ = child.kill();
    let _ = child.wait();

    let first = State::load().unwrap();
    assert!(!first.workspaces.contains_key("drop-kill"));
    let second = State::load().unwrap();
    assert!(!second.workspaces.contains_key("drop-kill"));
}

#[test]
#[serial]
fn kill_during_merge_blocks_mutations_until_explicit_resume_then_converges() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    for index in 0..600 {
        fs::write(base.path().join(format!("file-{index:04}")), b"old\n").unwrap();
    }
    assert!(forkspace(
        home.path(),
        &[
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "merge-kill",
            "--copy"
        ]
    )
    .status
    .success());
    let view = State::load()
        .unwrap()
        .require_workspace("merge-kill")
        .unwrap()
        .view_path
        .clone();
    for index in 0..600 {
        fs::write(view.join(format!("file-{index:04}")), b"new\n").unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args(["merge", "merge-kill", "--no-install", "--quiet"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let journal_dir = home.path().join("journal");
    assert!(wait_until(Duration::from_secs(15), || {
        fs::read_dir(&journal_dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                fs::read(entry.path())
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .and_then(|value| value["next"].as_u64())
                    .is_some_and(|next| next > 0 && next < 600)
            })
        })
    }));
    child.kill().unwrap();
    let _ = child.wait();

    let blocked = forkspace(home.path(), &["drop", "merge-kill"]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("merge merge-kill --resume"));
    let resumed = forkspace(
        home.path(),
        &["merge", "merge-kill", "--resume", "--no-install", "--quiet"],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    for index in 0..600 {
        assert_eq!(
            fs::read(base.path().join(format!("file-{index:04}"))).unwrap(),
            b"new\n"
        );
    }
    assert!(fs::read_dir(journal_dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true));
}

#[test]
#[serial]
fn unicode_non_utf8_case_and_long_paths_round_trip_through_copy_diff_and_merge() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let long = (0..30)
        .map(|index| format!("segment-{index:02}-abcdefghijklmnop"))
        .collect::<PathBuf>();
    let paths = [
        PathBuf::from("caf\u{e9}"),
        PathBuf::from("cafe\u{301}"),
        PathBuf::from("Case"),
        PathBuf::from("case"),
        PathBuf::from(std::ffi::OsString::from_vec(vec![b'n', b'o', b'n', 0xff])),
        long,
    ];
    for path in &paths {
        fs::create_dir_all(base.path().join(path).parent().unwrap()).unwrap();
        fs::write(base.path().join(path), b"old\n").unwrap();
    }
    assert!(forkspace(
        home.path(),
        &[
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "names",
            "--copy"
        ]
    )
    .status
    .success());
    let workspace = State::load()
        .unwrap()
        .require_workspace("names")
        .unwrap()
        .clone();
    for path in &paths {
        fs::write(workspace.view_path.join(path), b"new\n").unwrap();
    }
    let changes = select_backend(Some(BackendKind::PlainCopy))
        .unwrap()
        .changes(&workspace, true, false)
        .unwrap();
    for path in &paths {
        assert_eq!(
            changes.iter().filter(|change| &change.path == path).count(),
            1
        );
    }
    let merged = forkspace(home.path(), &["merge", "names", "--no-install", "--quiet"]);
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    for path in &paths {
        assert_eq!(fs::read(base.path().join(path)).unwrap(), b"new\n");
    }
}

#[test]
#[serial]
fn moved_base_is_flagged_but_the_independent_workspace_still_enters_and_diffs() {
    let home = common::setup_test_env();
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("base");
    fs::create_dir(&base).unwrap();
    fs::write(base.join("file"), b"old\n").unwrap();
    assert!(forkspace(
        home.path(),
        &["fork", base.to_str().unwrap(), "--name", "moved", "--copy"]
    )
    .status
    .success());
    let moved = root.path().join("renamed");
    fs::rename(&base, &moved).unwrap();

    let listed = forkspace(home.path(), &["ls", "--json"]);
    assert!(listed.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows[0]["backend"], "copy");
    assert_eq!(rows[0]["base_missing"], true);
    let entered = forkspace(home.path(), &["enter", "moved", "--", "cat", "file"]);
    assert!(entered.status.success());
    assert!(String::from_utf8_lossy(&entered.stdout).contains("old"));
    let view = State::load()
        .unwrap()
        .require_workspace("moved")
        .unwrap()
        .view_path
        .clone();
    fs::write(view.join("file"), b"workspace\n").unwrap();
    assert!(forkspace(home.path(), &["diff", "moved", "--stat"])
        .status
        .success());
    let merge = forkspace(home.path(), &["merge", "moved", "--no-install"]);
    assert!(!merge.status.success());
    assert!(String::from_utf8_lossy(&merge.stderr).contains("restore it before merging"));
}

#[test]
#[serial]
fn missing_or_ejected_workspace_volume_is_flagged_without_erasing_state() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"old\n").unwrap();
    assert!(forkspace(
        home.path(),
        &[
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "ejected",
            "--copy"
        ]
    )
    .status
    .success());
    let view = State::load().unwrap().workspaces["ejected"]
        .view_path
        .clone();
    let unavailable = view.with_file_name("ejected-volume-offline");
    fs::rename(&view, &unavailable).unwrap();

    let listed = forkspace(home.path(), &["ls", "--json"]);
    assert!(listed.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows[0]["view_missing"], true);
    let entered = forkspace(home.path(), &["enter", "ejected", "--", "true"]);
    assert_eq!(entered.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&entered.stderr);
    assert!(stderr.contains("recorded view path"));
    assert!(stderr.contains(&view.display().to_string()));
    assert!(State::load().unwrap().workspaces.contains_key("ejected"));

    fs::rename(unavailable, view).unwrap();
    assert!(forkspace(home.path(), &["enter", "ejected", "--", "true"])
        .status
        .success());
}

#[test]
#[serial]
fn backend_override_is_echoed_but_other_platform_state_is_never_repaired_away() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"old\n").unwrap();
    let forked = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .env("FORKSPACE_BACKEND", "copy")
        .args(["fork", base.path().to_str().unwrap(), "--name", "foreign"])
        .output()
        .unwrap();
    assert!(forked.status.success());
    assert!(String::from_utf8_lossy(&forked.stderr)
        .contains("FORKSPACE_BACKEND override selected 'copy'"));

    let state_path = home.path().join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["workspaces"]["foreign"]["backend_kind"] = serde_json::json!("mac-clone");
    state["workspaces"]["foreign"]["backend_state"] = serde_json::json!({"kind":"mac-clone"});
    fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

    let entered = forkspace(home.path(), &["enter", "foreign", "--", "true"]);
    assert_eq!(entered.status.code(), Some(7));
    let stderr = String::from_utf8_lossy(&entered.stderr);
    assert!(stderr.contains("mac-clone"));
    assert!(stderr.contains("macOS"));
    let listed = forkspace(home.path(), &["ls", "--json"]);
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("foreign"));
    assert_eq!(
        State::load().unwrap().workspaces["foreign"].backend_kind,
        BackendKind::MacClone
    );
}

#[test]
#[serial]
fn enospc_copy_fixture_cleans_partial_materialization_when_provided() {
    let Some(root) = std::env::var_os("FORKSPACE_TEST_ENOSPC_ROOT") else {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"enospc-volume\",\"reason\":\"set FORKSPACE_TEST_ENOSPC_ROOT to a writable volume with less than 32 MiB free\"}}"
        );
        return;
    };
    let root = PathBuf::from(root);
    let home = root.join(format!("forkspace-enospc-{}", std::process::id()));
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("large"), vec![7_u8; 32 * 1024 * 1024]).unwrap();
    let failed = forkspace(
        &home,
        &[
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "no-space",
            "--copy",
        ],
    );
    assert!(
        !failed.status.success(),
        "fixture volume was not constrained"
    );
    let listed = forkspace(&home, &["ls", "--json"]);
    assert!(listed.status.success());
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("no-space"));
    assert!(!home.join("workspaces/no-space").exists());
    let _ = fs::remove_dir_all(home);
}

#[test]
#[serial]
fn manifest_install_failure_keeps_the_view_and_reports_tracking_degraded() {
    let home = common::setup_test_env();
    fs::write(home.path().join("manifests"), b"not a directory").unwrap();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"payload\n").unwrap();
    let forked = forkspace(
        home.path(),
        &[
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "degraded-manifest",
            "--copy",
        ],
    );
    assert!(
        forked.status.success(),
        "{}",
        String::from_utf8_lossy(&forked.stderr)
    );
    assert!(String::from_utf8_lossy(&forked.stderr).contains("change tracking is degraded"));
    let state = State::load().unwrap();
    let workspace = state.require_workspace("degraded-manifest").unwrap();
    assert!(workspace.degraded);
    assert_eq!(
        workspace.manifest_status,
        forkspace::manifest::ManifestStatus::Degraded
    );
    assert_eq!(
        fs::read(workspace.view_path.join("file")).unwrap(),
        b"payload\n"
    );
    let listed = forkspace(home.path(), &["ls", "--json"]);
    assert!(listed.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows[0]["degraded"], true);
}

#[test]
#[serial]
fn sibling_cache_race_parallel_merges_and_diff_during_writes_remain_consistent() {
    if !Command::new("npm")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"npm\",\"reason\":\"npm is required for the shared-cache race\"}}"
        );
        return;
    }
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::create_dir(base.path().join("src")).unwrap();
    fs::write(
        base.path().join("package.json"),
        br#"{"name":"cache-race","version":"1.0.0","private":true}"#,
    )
    .unwrap();
    for index in 0..4 {
        fs::write(base.path().join(format!("src/file-{index}")), b"old\n").unwrap();
        assert!(forkspace(
            home.path(),
            &[
                "fork",
                base.path().to_str().unwrap(),
                "--name",
                &format!("sibling-{index}"),
                "--copy",
            ]
        )
        .status
        .success());
    }
    let mut installers = Vec::new();
    for index in 0..4 {
        let home = home.path().to_path_buf();
        installers.push(std::thread::spawn(move || {
            forkspace(
                &home,
                &[
                    "enter",
                    &format!("sibling-{index}"),
                    "--",
                    "npm",
                    "install",
                    "--ignore-scripts",
                ],
            )
        }));
    }
    for output in installers.into_iter().map(|worker| worker.join().unwrap()) {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let state = State::load().unwrap();
    for index in 0..4 {
        let lock = state.workspaces[&format!("sibling-{index}")]
            .view_path
            .join("package-lock.json");
        let _ = fs::remove_file(lock);
    }
    for index in 0..2 {
        let view = &state.workspaces[&format!("sibling-{index}")].view_path;
        fs::write(view.join(format!("src/file-{index}")), b"new\n").unwrap();
    }
    let writer_view = state.workspaces["sibling-2"].view_path.clone();
    let writer = std::thread::spawn(move || {
        for index in 0..100 {
            fs::write(writer_view.join("src/active-build"), index.to_string()).unwrap();
        }
    });
    for _ in 0..20 {
        let output = forkspace(home.path(), &["diff", "sibling-2", "--stat"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    writer.join().unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let mut mergers = Vec::new();
    for index in 0..2 {
        let home = home.path().to_path_buf();
        let barrier = barrier.clone();
        mergers.push(std::thread::spawn(move || {
            barrier.wait();
            forkspace(
                &home,
                &[
                    "merge",
                    &format!("sibling-{index}"),
                    "--no-install",
                    "--quiet",
                ],
            )
        }));
    }
    for output in mergers.into_iter().map(|worker| worker.join().unwrap()) {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(fs::read(base.path().join("src/file-0")).unwrap(), b"new\n");
    assert_eq!(fs::read(base.path().join("src/file-1")).unwrap(), b"new\n");
}
