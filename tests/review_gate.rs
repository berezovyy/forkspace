mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use forkspace::backend::select_backend;
use forkspace::changes::manifest::ManifestStatus;
use forkspace::error::ForkspaceError;
use forkspace::review::compare;
use forkspace::review::signals;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn copy_workspace(base: &Path, name: &str) -> Workspace {
    forkspace::workspace::create(
        base.to_path_buf(),
        name,
        forkspace::workspace::CreateOptions {
            force_copy: true,
            ..Default::default()
        },
    )
    .unwrap()
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
    assert!(json.contains("\"changes\":{\"all\":\"modified\""));

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
    let mut b = copy_workspace(second.path(), "other-b");
    b.name = "c".to_string();
    let mismatch = compare::build(&[a.clone(), b]).unwrap_err();
    assert!(matches!(&mismatch, ForkspaceError::BaseMismatch { .. }));
    // Bases are recorded canonically (macOS temporary directories sit under `/private`).
    let normalized = mismatch
        .to_string()
        .replace(
            &second.path().canonicalize().unwrap().display().to_string(),
            "<BASE-2>",
        )
        .replace(
            &first.path().canonicalize().unwrap().display().to_string(),
            "<BASE-1>",
        );
    assert_eq!(
        format!("{normalized}\n").as_bytes(),
        fixture("compare-base-mismatch.txt")
    );
    assert!(matches!(
        compare::build(&[a.clone(), a]),
        Err(ForkspaceError::InvalidArgument(_))
    ));

    let mut root = copy_workspace(first.path(), "same-root");
    let mut child = root.clone();
    root.name = "root-column".to_string();
    child.name = "child-column".to_string();
    child.parent = Some("parent".to_string());
    assert!(matches!(
        compare::build(&[root, child]),
        Err(ForkspaceError::BaseMismatch { .. })
    ));

    let usage = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["compare", "only-one"])
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stdout.is_empty());
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
    let yarn = b"# yarn lockfile v1\n\"left-pad@npm:^1.0.0\":\n  version \"1.0.0\"\n";
    assert!(signals::parse_lockfile(Path::new("yarn.lock"), yarn)
        .unwrap()
        .contains("left-pad"));
    assert!(signals::parse_lockfile(Path::new("package-lock.json"), b"{").is_none());
    assert!(signals::parse_lockfile(Path::new("bun.lockb"), b"binary").is_none());
    assert!(signals::parse_lockfile(
        Path::new("pnpm-lock.yaml"),
        b"lockfileVersion: 'future'\n/packages:\n  /wrong@1:\n"
    )
    .is_none());

    let capped = signals::Signals {
        deps_added: 6,
        dep_names_added: ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ..signals::Signals::default()
    };
    assert_eq!(
        signals::render(&capped).as_deref(),
        Some("signals: +6 dependencies (a, b, c, d, e, +1 more)")
    );
    assert_eq!(
        signals::render(&signals::Signals {
            deps_removed: 1,
            dep_names_removed: ["old".to_string()].into_iter().collect(),
            ..signals::Signals::default()
        })
        .as_deref(),
        Some("signals: -1 dependency (old)")
    );
    assert_eq!(
        signals::render(&signals::Signals {
            tests_deleted: 1,
            tests_touched: 1,
            ..signals::Signals::default()
        })
        .as_deref(),
        Some("signals: 1 test file deleted")
    );
    assert_eq!(
        signals::render(&signals::Signals {
            new_files: 5,
            edited_files: 1,
            ..signals::Signals::default()
        })
        .as_deref(),
        Some("signals: 5 new / 1 edited")
    );

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
        Some("signals: +1 dependency (left-pad) · 1 new / 1 edited · 1 test file deleted")
    );

    let quiet_facts = signals::Signals {
        edited_files: 2,
        ..signals::Signals::default()
    };
    assert!(signals::render(&quiet_facts).is_none());

    let diff = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["diff", "review", "--stat"])
        .env("FORKSPACE_HOME", home.path())
        .output()
        .unwrap();
    assert!(diff.status.success());
    assert_eq!(
        diff.stdout,
        fixture("diff-stat-signals.txt"),
        "diff signals golden changed"
    );

    let dry = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["merge", "review", "--dry-run", "--no-install"])
        .env("FORKSPACE_HOME", home.path())
        .output()
        .unwrap();
    assert!(dry.status.success());
    let dry_plan = String::from_utf8(dry.stdout).unwrap();
    assert_eq!(dry_plan.as_bytes(), fixture("merge-plan-signals.txt"));

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
    assert_eq!(merged.stdout, fixture("merge-quiet.txt"));
    let stdout = String::from_utf8_lossy(&merged.stdout);
    assert!(!stdout.contains("merge plan"));
    assert!(stdout.contains("merged 4 files"));
}

#[test]
fn named_phase_16_fixtures_track_the_cli_contract_goldens() {
    assert_eq!(
        fixture("compare-three.txt"),
        fixture("compare-three.stdout")
    );
    assert_eq!(
        fixture("compare-files.txt"),
        fixture("compare-files.stdout")
    );
    assert_eq!(fixture("compare-json.json"), fixture("compare-json.stdout"));
}

#[test]
#[serial]
fn dry_run_plan_is_the_exact_prefix_of_default_merge_output() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("edited"), b"old\n").unwrap();
    let workspace = copy_workspace(base.path(), "same-plan");
    fs::write(workspace.view_path.join("edited"), b"new\n").unwrap();
    let dry = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["merge", "same-plan", "--dry-run", "--no-install"])
        .env("FORKSPACE_HOME", home.path())
        .output()
        .unwrap();
    assert!(dry.status.success());
    let applied = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["merge", "same-plan", "--no-install"])
        .env("FORKSPACE_HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(applied.status.success());
    assert!(applied.stdout.starts_with(&dry.stdout));
    assert_eq!(
        &applied.stdout[dry.stdout.len()..],
        b"applying\xe2\x80\xa6\nmerged 1 files (0 added, 1 modified, 0 deleted)\n"
    );
}

#[test]
#[serial]
fn set_edge_cases_filters_and_completion_contract_stay_mechanical() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("present"), b"old\n").unwrap();
    fs::write(base.path().join(".gitignore"), b"ignored\n").unwrap();
    let a_view = tempfile::tempdir().unwrap();
    let b_view = tempfile::tempdir().unwrap();
    fs::write(a_view.path().join(".gitignore"), b"ignored\n").unwrap();
    fs::write(b_view.path().join(".gitignore"), b"ignored\n").unwrap();
    fs::write(a_view.path().join("present"), b"old\n").unwrap();
    fs::write(b_view.path().join("present"), b"old\n").unwrap();
    let a = synthetic_workspace(base.path(), a_view.path(), "edge-a");
    let b = synthetic_workspace(base.path(), b_view.path(), "edge-b");
    fs::write(a_view.path().join("ignored"), b"a\n").unwrap();
    fs::write(b_view.path().join("ignored"), b"b\n").unwrap();
    fs::rename(a_view.path().join("present"), a_view.path().join("renamed")).unwrap();
    fs::rename(b_view.path().join("present"), b_view.path().join("renamed")).unwrap();
    let output = compare::build(&[a, b]).unwrap();
    assert!(!output
        .contested
        .iter()
        .any(|path| path == Path::new("ignored")));
    assert!(output.contested.contains(&PathBuf::from("present")));
    assert!(output.contested.contains(&PathBuf::from("renamed")));
    assert!(output.partial.is_empty());
    assert!(!compare::render(&output).contains("some but not all"));

    let add_view = tempfile::tempdir().unwrap();
    let add = synthetic_workspace(base.path(), add_view.path(), "edge-add");
    fs::write(add_view.path().join("kind-split"), b"added\n").unwrap();
    fs::write(base.path().join("kind-split"), b"base\n").unwrap();
    let delete_view = tempfile::tempdir().unwrap();
    fs::write(delete_view.path().join("kind-split"), b"base\n").unwrap();
    let delete = synthetic_workspace(base.path(), delete_view.path(), "edge-delete");
    fs::remove_file(delete_view.path().join("kind-split")).unwrap();
    let kinds = compare::build(&[add, delete]).unwrap();
    assert_eq!(kinds.contested, [PathBuf::from("kind-split")]);
    let files = compare::render_files(&kinds);
    assert!(files.contains("kind-split"));
    assert!(files.contains(" A"));
    assert!(files.contains(" D"));

    let completion = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    let text = String::from_utf8(completion.stdout).unwrap();
    assert!(text.contains("compare)"));
    assert!(text.contains("'*:workspace:_forkspace_workspaces'"));
    assert!(text.contains("--json"));
    assert!(text.contains("--files"));
    assert!(text.contains("--quiet"));
}

fn synthetic_workspace(base: &Path, view: &Path, name: &str) -> Workspace {
    let mut workspace = Workspace::new(name, base.to_path_buf(), None);
    workspace.backend_kind = forkspace::backend::BackendKind::PlainCopy;
    workspace.backend_state = forkspace::backend::BackendState::CowDir {
        reflinked_files: 0,
        copied_files: 1,
        unknown_fields: serde_json::Map::new(),
    };
    workspace.view_path = view.to_path_buf();
    std::thread::sleep(std::time::Duration::from_millis(2));
    workspace.fork_time_ns = forkspace::util::now_unix_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    forkspace::changes::manifest::build(name, view, workspace.fork_time_ns).unwrap();
    workspace
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cli-contract")
            .join(name),
    )
    .unwrap()
}
