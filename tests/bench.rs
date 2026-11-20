//! The benchmark script, in its short mode.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_forkspace");

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
}
