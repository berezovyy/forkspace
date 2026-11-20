//! The jobserver shared by all workspaces (`src/process/jobserver.rs`): commands started by
//! `enter` get `CARGO_MAKEFLAGS` naming one pipe that holds one job slot per CPU.

#![cfg(target_os = "linux")]

mod common;

use common::cli::{stdout, write, Env};
use serial_test::serial;

/// Run `script` in workspace `w` and return the line it prints (not `enter`'s own output).
fn run(env: &Env, script: &str) -> String {
    let output = env
        .command(&["enter", "w", "--", "sh", "-c", script])
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout(&output)
        .lines()
        .find(|line| line.starts_with("result:"))
        .unwrap_or_default()
        .to_string()
}

/// Take every slot without blocking and print how many there were.
const TAKE_ALL: &str = r#"f="${CARGO_MAKEFLAGS#*fifo:}"; n=$(dd if="$f" bs=1 count=4096 iflag=nonblock 2>/dev/null | wc -c); echo "result:$CARGO_MAKEFLAGS:$n""#;

#[test]
#[serial]
fn enter_commands_share_a_fifo_jobserver_with_a_slot_per_cpu() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("w");
    let cpus = std::thread::available_parallelism().unwrap().get();
    let taken = run(&env, TAKE_ALL);
    assert!(taken.contains("-j --jobserver-auth=fifo:"), "{taken}");
    assert!(taken.ends_with(&format!(":{cpus}")), "{taken}");
    // Slots a command took and never returned are back once no command holds the pipe.
    let again = run(&env, TAKE_ALL);
    assert!(again.ends_with(&format!(":{cpus}")), "{again}");
}

/// A joiner that finds slots left in the pipe by a holder on its way out keeps the slot
/// count exact instead of adding to them.
#[test]
#[serial]
fn a_first_holder_replaces_leftover_slots_instead_of_adding_to_them() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("w");
    let cpus = std::thread::available_parallelism().unwrap().get();
    // Learn where the pipe is, then hold it open with extra slots in it, as an exiting
    // holder that has not closed it yet would.
    let taken = run(&env, TAKE_ALL);
    // "result:<CARGO_MAKEFLAGS>:<count>", the flags ending in "--jobserver-auth=fifo:<path>".
    let fifo = taken
        .split_once("--jobserver-auth=fifo:")
        .and_then(|(_, rest)| rest.rsplit_once(':'))
        .map(|(path, _)| path.to_string())
        .unwrap();
    let mut held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .unwrap();
    std::io::Write::write_all(&mut held, &[b'+'; 5]).unwrap();
    let again = run(&env, TAKE_ALL);
    assert!(again.ends_with(&format!(":{cpus}")), "{again}");
    drop(held);
}
