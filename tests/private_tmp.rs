//! Each workspace's commands get a private `TMPDIR` (`src/workspace/tmp.rs`), writable inside
//! the sandbox and removed with the workspace.

mod common;

use std::path::PathBuf;

use common::cli::{stdout, write, Env};
use serial_test::serial;

fn tmpdir_of(env: &Env, name: &str) -> PathBuf {
    let output = env.ok(&[
        "enter",
        name,
        "--",
        "sh",
        "-c",
        "touch \"$TMPDIR/made-here\" && printf '%s\\n' \"$TMPDIR\" \"$TMP\" \"$TEMP\"",
    ]);
    let printed = stdout(&output);
    let lines: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with('/'))
        .collect();
    assert_eq!(lines.len(), 3, "{printed}");
    assert!(lines.iter().all(|line| *line == lines[0]), "{printed}");
    PathBuf::from(lines[0])
}

#[test]
#[serial]
fn workspaces_get_separate_writable_tmpdirs_that_drop_removes() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("one");
    env.fork_copy("two");
    let one = tmpdir_of(&env, "one");
    let two = tmpdir_of(&env, "two");
    assert_ne!(one, two);
    assert!(
        one.join("made-here").exists(),
        "the sandbox refused the private TMPDIR"
    );
    assert!(
        !one.starts_with(env.view("one")),
        "temporary files would be workspace changes"
    );
    let diff = stdout(&env.ok(&["diff", "one", "--files"]));
    assert!(!diff.contains("made-here"), "{diff}");

    env.ok(&["drop", "one"]);
    assert!(!one.exists());
    assert!(two.exists());
    env.ok(&["drop", "two"]);
    assert!(!two.exists());
}

#[test]
#[serial]
fn the_private_tmpdir_can_be_turned_off() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("w");
    let output = env
        .command(&["enter", "w", "--", "sh", "-c", "printf '%s' \"$TMPDIR\""])
        .env("FORKSPACE_PRIVATE_TMP", "0")
        .env("TMPDIR", "/tmp")
        .output()
        .unwrap();
    assert!(stdout(&output).starts_with("/tmp"), "{}", stdout(&output));
    assert!(!stdout(&output).starts_with("/tmp/forkspace-"));
}
