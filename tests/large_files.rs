//! Files over the text-diff limit are never diffed line by line, which holds both versions in
//! memory several times over (DIFF-11): `diff` shows no line counts for them, `diff --full` a
//! note, and `merge --patch` refuses with nothing printed; `merge` still carries them.

mod common;

use std::fs;

use common::cli::{stderr, stdout, write, Env};
use forkspace::changes::MAX_TEXT_DIFF_BYTES;
use serial_test::serial;

/// Text lines filling just over the limit, with `marker` as the first line.
fn big_text(marker: &str) -> String {
    let line = "0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456\n";
    let count = (MAX_TEXT_DIFF_BYTES as usize) / line.len() + 2;
    let mut text = format!("{marker}\n");
    text.reserve(count * line.len());
    for _ in 0..count {
        text.push_str(line);
    }
    text
}

#[test]
#[serial]
fn a_file_over_the_text_diff_limit_is_not_diffed_line_by_line() {
    let env = Env::new();
    write(&env.project().join("big.txt"), &big_text("first"));
    write(&env.project().join("small.txt"), "small\n");
    env.fork_copy("ws");
    let view = env.view("ws");
    write(&view.join("big.txt"), &big_text("changed"));
    write(&view.join("small.txt"), "small edited\n");

    let listed = stdout(&env.ok(&["diff", "ws"]));
    let big_line = listed
        .lines()
        .find(|line| line.contains("big.txt"))
        .unwrap();
    assert!(!big_line.contains('('), "{listed}");
    assert!(listed.contains("small.txt (+1) (-1)"), "{listed}");

    let full = stdout(&env.ok(&["diff", "ws", "--full"]));
    assert!(
        full.contains("diff not shown"),
        "{}",
        &full[..full.len().min(2000)]
    );
    assert!(
        full.contains("+small edited"),
        "{}",
        &full[..full.len().min(2000)]
    );
    assert!(full.len() < 10_000, "{} bytes of diff", full.len());

    let patch = env.run(&["merge", "ws", "--patch"]);
    assert!(!patch.status.success());
    assert!(
        patch.stdout.is_empty(),
        "{} bytes of patch",
        patch.stdout.len()
    );
    assert!(stderr(&patch).contains("big.txt"), "{}", stderr(&patch));
    assert!(stderr(&patch).contains("MiB"), "{}", stderr(&patch));

    env.ok(&["merge", "ws", "--no-install"]);
    assert_eq!(
        fs::read(env.project().join("big.txt")).unwrap(),
        fs::read(view.join("big.txt")).unwrap()
    );
}
