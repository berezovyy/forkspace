//! Commands find their workspace without a name: the one whose folder you are in, else the
//! only one there is (`src/workspace/pick.rs`).

mod common;

use common::cli::{stderr, stdout, write, Env};
use serial_test::serial;

#[test]
#[serial]
fn inside_a_workspace_commands_need_no_name() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("one");
    env.fork_copy("two");
    write(&env.view("two").join("b.txt"), "b\n");
    let inside = env.view("two").join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    let output = env
        .command(&["diff"])
        .current_dir(&inside)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("b.txt"), "{}", stdout(&output));
}

#[test]
#[serial]
fn outside_the_only_workspace_is_used_and_several_must_be_named() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    let none = env.run(&["diff"]);
    assert!(!none.status.success());
    assert!(stderr(&none).contains("forkspace new"), "{}", stderr(&none));

    env.fork_copy("one");
    write(&env.view("one").join("b.txt"), "b\n");
    let only = env.run(&["diff"]);
    assert!(only.status.success(), "{}", stderr(&only));
    assert!(stdout(&only).contains("b.txt"));

    env.fork_copy("two");
    let several = env.run(&["diff"]);
    assert!(!several.status.success());
    assert!(
        stderr(&several).contains("one, two"),
        "{}",
        stderr(&several)
    );
}
