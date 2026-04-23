mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use forkspace::backend::select_backend;
use forkspace::commands::fork::create_workspace_with_copy;
use forkspace::compare;
use forkspace::error::ForkspaceError;
use forkspace::manifest::ManifestStatus;
use forkspace::signals;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn copy_workspace(base: &Path, name: &str) -> Workspace {
    create_workspace_with_copy(base.to_path_buf(), name, None, None, false, None, true).unwrap()
}

#[test]
#[serial]
fn compare_preserves_argument_order_and_buckets_divergence() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    for name in ["all", "partial", "delete"] {
        fs::write(base.path().join(name), b"old\n").unwrap();
    }
    let a = copy_workspace(base.path(), "attempt-a");
    let b = copy_workspace(base.path(), "attempt-b");
    let c = copy_workspace(base.path(), "attempt-c");
    std::thread::sleep(std::time::Duration::from_millis(2));
    for workspace in [&a, &b, &c] {
        fs::write(workspace.view_path.join("all"), workspace.name.as_bytes()).unwrap();
    }
    for workspace in [&a, &b] {
        fs::write(
            workspace.view_path.join("partial"),
            workspace.name.as_bytes(),
        )
        .unwrap();
    }
    fs::write(a.view_path.join("only-a"), b"a").unwrap();
    fs::remove_file(c.view_path.join("delete")).unwrap();

    let output = compare::build(&[b.clone(), a.clone(), c.clone()]).unwrap();
    assert_eq!(
        output
            .workspaces
            .iter()
            .map(|row| row.workspace.as_str())
            .collect::<Vec<_>>(),
        ["attempt-b", "attempt-a", "attempt-c"]
    );
    assert_eq!(output.contested, [PathBuf::from("all")]);
    assert_eq!(output.partial, [PathBuf::from("partial")]);
    assert_eq!(output.unique[1].1, [PathBuf::from("only-a")]);
    assert_eq!(output.unique[2].1, [PathBuf::from("delete")]);
    let table = compare::render(&output);
    assert!(table.contains("contested (all 3): all"));
    assert!(table.contains("unique to attempt-a: only-a"));
    let files = compare::render_files(&output);
    assert!(files.contains("delete  -  -  D"));
    let json = serde_json::to_string(&output).unwrap();
    assert!(json.contains("\"partial\":[\"partial\"]"));

    let mut degraded = c.clone();
    degraded.manifest_status = ManifestStatus::Building;
    let output = compare::build(&[a, b, degraded]).unwrap();
    assert_eq!(output.degraded, ["attempt-c"]);
}

#[test]
#[serial]
fn compare_refuses_duplicate_and_cross_base_inputs_before_scanning() {
    let _home = common::setup_test_env();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let a = copy_workspace(first.path(), "same-a");
    let b = copy_workspace(second.path(), "other-b");
    assert!(matches!(
        compare::build(&[a.clone(), b]),
        Err(ForkspaceError::BaseMismatch { .. })
    ));
    assert!(matches!(
        compare::build(&[a.clone(), a]),
        Err(ForkspaceError::InvalidArgument(_))
    ));
}

#[test]
fn signal_parsers_and_test_path_heuristic_are_explicit() {
    let npm = br#"{"lockfileVersion":3,"packages":{"":"", "node_modules/left-pad":{},"node_modules/@scope/pkg":{}}}"#;
    assert_eq!(
        signals::parse_lockfile(Path::new("package-lock.json"), npm)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        ["@scope/pkg", "left-pad"]
    );
    let cargo = b"version = 4\n[[package]]\nname = \"serde\"\n[[package]]\nname = \"time\"\n";
    assert_eq!(
        signals::parse_lockfile(Path::new("Cargo.lock"), cargo)
            .unwrap()
            .len(),
        2
    );
    let pnpm = b"lockfileVersion: '9'\n/packages:\n  /left-pad@1.0.0:\n";
    assert!(signals::parse_lockfile(Path::new("pnpm-lock.yaml"), pnpm)
        .unwrap()
        .contains("left-pad"));
    assert!(signals::parse_lockfile(Path::new("package-lock.json"), b"{").is_none());
    assert!(signals::parse_lockfile(Path::new("bun.lockb"), b"binary").is_none());

    for positive in [
        "foo.test.ts",
        "foo.spec.js",
        "handler_test.go",
        "test_parse.py",
        "tests/e2e.rs",
        "__tests__/a.tsx",
        "spec/models/user_spec.rb",
    ] {
        assert!(signals::is_test_path(Path::new(positive)), "{positive}");
    }
    for negative in ["latest.ts", "spectrum.css", "contest/main.c", "src/lib.rs"] {
        // src/lib.rs containing #[cfg(test)] is intentionally a documented known miss.
        assert!(!signals::is_test_path(Path::new(negative)), "{negative}");
    }
}

#[test]
#[serial]
fn diff_signals_and_merge_plan_are_factual_and_quiet_is_available() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("edited"), b"old\n").unwrap();
    fs::write(base.path().join("gone.test.ts"), b"test\n").unwrap();
    fs::write(
        base.path().join("package-lock.json"),
        br#"{"lockfileVersion":3,"packages":{"":{}}}"#,
    )
    .unwrap();
    let workspace = copy_workspace(base.path(), "review");
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(workspace.view_path.join("edited"), b"new\n").unwrap();
    fs::write(workspace.view_path.join("new-file"), b"new\n").unwrap();
    fs::remove_file(workspace.view_path.join("gone.test.ts")).unwrap();
    fs::write(
        workspace.view_path.join("package-lock.json"),
        br#"{"lockfileVersion":3,"packages":{"":{},"node_modules/left-pad":{}}}"#,
    )
    .unwrap();
    let changes = select_backend(Some(workspace.backend_kind))
        .unwrap()
        .changes(&workspace, false, false)
        .unwrap();
    let signal = signals::collect(&workspace, &changes);
    assert_eq!(
        signals::render(&signal).as_deref(),
        Some("signals: +1 dependency (left-pad) · 1 new / 1 edited · 1 test files deleted")
    );

    let diff = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["diff", "review", "--stat"])
        .env("FORKSPACE_HOME", home.path())
        .output()
        .unwrap();
    assert!(diff.status.success());
    assert!(String::from_utf8_lossy(&diff.stdout).contains("signals: +1 dependency"));

    let dry = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["merge", "review", "--dry-run", "--no-install"])
        .env("FORKSPACE_HOME", home.path())
        .output()
        .unwrap();
    assert!(dry.status.success());
    assert!(String::from_utf8_lossy(&dry.stdout).contains("signals: +1 dependency"));

    let merged = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["merge", "review", "--quiet", "--no-install"])
        .env("FORKSPACE_HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    let stdout = String::from_utf8_lossy(&merged.stdout);
    assert!(!stdout.contains("merge plan"));
    assert!(stdout.contains("merged 4 files"));
}
