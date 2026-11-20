//! The jobserver shared by all workspaces (`src/process/jobserver.rs`): commands started by
//! `enter` get `CARGO_MAKEFLAGS` naming one pipe that holds `FORKSPACE_JOBS` job slots.

#![cfg(target_os = "linux")]

mod common;

use common::cli::{stdout, write, Env};
use serial_test::serial;

/// Run `script` in workspace `w` and return the line it prints (not `enter`'s own output).
fn run(env: &Env, script: &str, vars: &[(&str, &str)]) -> String {
    let mut command = env.command(&["enter", "w", "--", "sh", "-c", script]);
    command
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS");
    for (key, value) in vars {
        command.env(key, value);
    }
    let output = command.output().unwrap();
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

#[test]
#[serial]
fn enter_commands_share_a_fifo_jobserver_with_the_configured_slots() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("w");
    // Take every slot without blocking: exactly FORKSPACE_JOBS are there.
    let taken = run(
        &env,
        r#"f="${CARGO_MAKEFLAGS#*fifo:}"; n=$(dd if="$f" bs=1 count=64 iflag=nonblock 2>/dev/null | wc -c); echo "result:$CARGO_MAKEFLAGS:$n""#,
        &[("FORKSPACE_JOBS", "3")],
    );
    assert!(taken.contains("-j --jobserver-auth=fifo:"), "{taken}");
    assert!(taken.ends_with(":3"), "{taken}");
    // Slots a command took and never returned are back once no command holds the pipe.
    let again = run(
        &env,
        r#"f="${CARGO_MAKEFLAGS#*fifo:}"; n=$(dd if="$f" bs=1 count=64 iflag=nonblock 2>/dev/null | wc -c); echo "result:$n""#,
        &[("FORKSPACE_JOBS", "3")],
    );
    assert_eq!(again, "result:3");
}

#[test]
#[serial]
fn the_jobserver_can_be_turned_off() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("w");
    let flags = run(
        &env,
        r#"echo "result:${CARGO_MAKEFLAGS:-none}""#,
        &[("FORKSPACE_JOBSERVER", "off")],
    );
    assert_eq!(flags, "result:none");
}
