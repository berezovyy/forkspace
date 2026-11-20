//! Inputs at the edges of what the system allows: paths near PATH_MAX, signals during a
//! fork, set-group-ID directories.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::cli::{stderr, write, Env};
use serial_test::serial;

/// A relative path of about 3,900 bytes: fine under the project, over PATH_MAX (4,096) once
/// placed under a 500-byte `FORKSPACE_HOME`.
fn deep_relative() -> std::path::PathBuf {
    (0..16).map(|_| "d".repeat(240)).collect()
}

#[test]
#[serial]
fn a_path_too_long_for_forkspace_home_fails_cleanly_and_names_the_remedy() {
    let env = Env::new();
    let project = env.project();
    write(&project.join(deep_relative()).join("f.txt"), "deep\n");
    write(&project.join("top.txt"), "top\n");
    let home = env.outside().join("h".repeat(200)).join("h".repeat(200));
    fs::create_dir_all(&home).unwrap();
    let output = env
        .command(&["new", "deep"])
        .env("FORKSPACE_HOME", &home)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("too long to copy under FORKSPACE_HOME"),
        "{message}"
    );
    assert!(
        message.len() < 1_000,
        "the whole path was printed: {} bytes",
        message.len()
    );
    // Nothing of the attempt is left: no workspace, no partial copy.
    let workspaces = home.join("workspaces");
    assert_eq!(
        fs::read_dir(&workspaces)
            .map(|entries| entries.count())
            .unwrap_or(0),
        0
    );
    // Excluding the path makes the fork possible.
    write(&project.join(".forkspaceexclude"), "/d*/\n");
    let output = env
        .command(&["new", "deep"])
        .env("FORKSPACE_HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
#[serial]
fn a_fork_interrupted_by_a_signal_leaves_nothing_behind() {
    let env = Env::new();
    let project = env.project();
    for index in 0..4_000 {
        write(
            &project.join(format!("d{}/f{index}.txt", index % 40)),
            "x\n",
        );
    }
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut child = env.command(&["new", "interrupted"]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(15));
        // SAFETY: `kill` only signals the child we started.
        unsafe {
            libc::kill(child.id() as libc::pid_t, signal);
        }
        let status = child.wait().unwrap();
        // The next command reconciles whatever the interrupted one left.
        let names = env.workspaces().join("\n");
        if status.success() {
            assert!(names.contains("interrupted"));
            env.ok(&["drop", "interrupted"]);
        } else {
            assert!(!names.contains("interrupted"), "{names}");
        }
        // The name is free again and a fork under it works.
        env.fork_copy("interrupted");
        env.ok(&["drop", "interrupted"]);
        let leftovers: Vec<_> = fs::read_dir(env.home.path().join("workspaces"))
            .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
#[serial]
fn set_group_id_directories_keep_the_bit_and_are_no_change() {
    let env = Env::new();
    let project = env.project();
    write(&project.join("shared/file.txt"), "x\n");
    let shared = project.join("shared");
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o2775)).unwrap();
    let mode = fs::metadata(&shared).unwrap().permissions().mode() & 0o7777;
    if mode & 0o2000 == 0 {
        eprintln!("FORKSPACE_TEST_SKIP {{\"capability\":\"setgid\",\"reason\":\"filesystem drops S_ISGID\"}}");
        return;
    }
    env.ok(&["new", "copied"]);
    env.ok(&["new", "default"]);
    for name in ["copied", "default"] {
        let view = env.view(name);
        let copied = fs::metadata(view.join("shared"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(copied, mode, "{name}");
        let diff = env.ok(&["diff", name]);
        let diff = String::from_utf8_lossy(&diff.stdout).into_owned();
        assert!(
            diff.trim().is_empty() || diff.contains("clean"),
            "{name}: {diff}"
        );
    }
}

/// A tool deleting part of the project while it is forked (`git gc` packing loose objects, a
/// package manager replacing a dependency) used to fail the fork with "failed to walk ...:
/// No such file or directory". What was deleted is simply not in the workspace.
#[test]
#[serial]
fn a_fork_succeeds_while_the_project_deletes_a_directory() {
    let env = Env::new();
    write(&env.project().join("keep.txt"), "keep\n");
    for round in 0..6 {
        let doomed = env.project().join("doomed");
        for index in 0..300 {
            write(&doomed.join(format!("d{index}/f.txt")), "x\n");
            std::os::unix::fs::symlink("f.txt", doomed.join(format!("d{index}/link"))).unwrap();
        }
        let deleter = {
            let doomed = doomed.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1 + round * 3));
                fs::remove_dir_all(&doomed).unwrap();
            })
        };
        let name = format!("racing-{round}");
        let output = env.run(&["new", &name]);
        deleter.join().unwrap();
        assert!(
            output.status.success(),
            "round {round}: {}",
            stderr(&output)
        );
        assert!(env.view(&name).join("keep.txt").exists());
        let diff = env.ok(&["diff", &name]);
        let diff = String::from_utf8_lossy(&diff.stdout).into_owned();
        assert!(
            diff.trim().is_empty() || diff.contains("clean"),
            "round {round}: {diff}"
        );
    }
}
