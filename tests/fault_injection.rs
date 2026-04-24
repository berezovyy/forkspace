mod common;

use std::fs;
use std::sync::{Arc, Barrier};

use forkspace::error::ForkspaceError;
use forkspace::state::State;
use forkspace::workspace::Workspace;
use serial_test::serial;

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
