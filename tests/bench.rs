//! `FORKSPACE_TRACE` phase timings and the benchmark script that reads them.

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_forkspace");

fn forkspace(home: &Path, trace: bool, args: &[&str]) -> Output {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env("FORKSPACE_HOME", home)
        .env("FORKSPACE_BACKEND", "copy")
        .env_remove("FORKSPACE_TRACE");
    if trace {
        command.env("FORKSPACE_TRACE", "1");
    }
    let output = command.output().expect("run forkspace");
    assert!(
        output.status.success(),
        "forkspace {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn trace_phases(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix("forkspace-trace: "))
        .map(|rest| rest.split(' ').next().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn trace_lines_go_to_stderr_only() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("file.txt"), "one\n").unwrap();
    let from = project.path().to_str().unwrap();

    let created = forkspace(home.path(), true, &["new", "ws", "--from", from]);
    let phases = trace_phases(&created);
    for phase in ["preflight", "materialize", "manifest", "state", "command"] {
        assert!(
            phases.iter().any(|p| p == phase),
            "missing {phase} in {phases:?}"
        );
    }

    let view = home.path().join("workspaces/ws");
    std::fs::write(view.join("file.txt"), "two\n").unwrap();
    let commands: [&[&str]; 4] = [
        &["diff", "ws"],
        &["merge", "ws", "--dry-run"],
        &["ls"],
        &["drop", "ws"],
    ];
    for args in commands {
        let output = forkspace(home.path(), true, args);
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("forkspace-trace"),
            "trace reached stdout for {args:?}"
        );
        assert!(trace_phases(&output).contains(&"command".to_string()));
    }

    let quiet = forkspace(home.path(), false, &["new", "ws2", "--from", from]);
    assert!(trace_phases(&quiet).is_empty());
}

#[test]
fn bench_script_runs_in_short_mode() {
    let python = Command::new("python3").arg("--version").output();
    if !python.is_ok_and(|output| output.status.success()) {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/bench.py");
    let output = Command::new("python3")
        .arg(script)
        .args(["--short", "--json", "--backends", "copy", "--bin", BIN])
        .arg("--work")
        .arg(work.path())
        .env_remove("FORKSPACE_HOME")
        .output()
        .expect("run bench.py");
    assert!(
        output.status.success(),
        "bench.py failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let backend = &report["backends"][0];
    assert_eq!(backend["kind"], "copy");
    assert_eq!(backend["errors"], serde_json::json!([]));
    for label in ["new", "diff --stat (clean)", "merge (one file)", "drop"] {
        assert_eq!(
            backend["summary"][label]["n"], 2,
            "missing samples for {label}"
        );
    }
    assert!(backend["phases"]["new"]["materialize"]["p50"].is_number());
}
