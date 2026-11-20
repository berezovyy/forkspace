//! Journal steps act only on what their plan saw: never through a parent that became a symlink
//! (FS-01, FS-02), never over an entry written after planning (PULL-09, MERGE-18), and an
//! interrupted operation re-checks the base when it resumes (MERGE-20, PULL-10).

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use forkspace::state::State;
use forkspace::sync::{journal, merge, plan, pull};
use forkspace::workspace::Workspace;
use serial_test::serial;

struct Env {
    home: common::TestHome,
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            home: common::setup_test_env(),
            root: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(env.project()).unwrap();
        fs::create_dir(env.outside()).unwrap();
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    /// A directory outside every tree forkspace may write.
    fn outside(&self) -> PathBuf {
        self.root.path().join("outside")
    }

    fn view(&self, name: &str) -> PathBuf {
        self.workspace(name).view_path
    }

    fn workspace(&self, name: &str) -> Workspace {
        State::load()
            .unwrap()
            .require_workspace(name)
            .unwrap()
            .clone()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forkspace"));
        command
            .env("FORKSPACE_HOME", self.home.path())
            .current_dir(self.project())
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn fork(&self, name: &str) {
        self.ok(&["new", name]);
    }

    /// Start a command whose journal steps each wait about `delay_ms` first; `None` where
    /// that cannot be done (see [`common::with_slow_renames`]).
    fn spawn_slow(&self, args: &[&str], delay_ms: u64) -> Option<Child> {
        let command = self.command(args);
        let mut slowed = common::with_slow_renames(&command, Duration::from_millis(delay_ms))?;
        Some(
            slowed
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    /// Wait until an open journal of \`op\` records at least \`next\` applied steps.
    fn wait_for_journal(&self, op: &str, next: u64) {
        let directory = self.home.path().join("journal");
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let reached = fs::read_dir(&directory).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.starts_with(op)
                        && name.ends_with(".json")
                        && fs::read(entry.path())
                            .ok()
                            .and_then(|bytes| {
                                serde_json::from_slice::<serde_json::Value>(&bytes).ok()
                            })
                            .and_then(|value| value["next"].as_u64())
                            .is_some_and(|value| value >= next)
                })
            });
            if reached {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("no {op} journal reached step {next}");
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("<{error}>"))
}

/// Every file under \`root\` with its content, sorted.
fn files(root: &Path) -> Vec<(PathBuf, String)> {
    let mut found: Vec<_> = walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .flatten()
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                read(entry.path()),
            )
        })
        .collect();
    found.sort();
    found
}

fn sentinels(env: &Env) {
    write(&env.outside().join("x.txt"), "SENTINEL x\n");
    write(&env.outside().join("y.txt"), "SENTINEL y\n");
}

fn sentinels_intact(env: &Env) {
    assert_eq!(
        files(&env.outside()),
        vec![
            (PathBuf::from("x.txt"), "SENTINEL x\n".to_string()),
            (PathBuf::from("y.txt"), "SENTINEL y\n".to_string()),
        ],
        "a directory outside the tree was changed"
    );
}

/// FS-01: the view replaces directory \`d\` with a symlink to a directory outside the tree
/// that holds entries of the same names.
fn replace_dir_with_outside_link(env: &Env, root: &Path) {
    fs::remove_file(root.join("d/x.txt")).unwrap();
    fs::remove_file(root.join("d/y.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();
    symlink(env.outside(), root.join("d")).unwrap();
}

#[test]
#[serial]
fn merge_and_undo_never_act_through_a_directory_replaced_by_a_symlink() {
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "base x\n");
    write(&env.project().join("d/y.txt"), "base y\n");
    write(&env.project().join("keep.txt"), "keep\n");
    sentinels(&env);
    env.fork("w");
    replace_dir_with_outside_link(&env, &env.view("w"));

    env.ok(&["merge", "w", "--quiet", "--no-install"]);
    sentinels_intact(&env);
    assert_eq!(
        fs::read_link(env.project().join("d")).unwrap(),
        env.outside()
    );

    env.ok(&["merge", "--undo", "w", "--no-install"]);
    sentinels_intact(&env);
    assert!(fs::symlink_metadata(env.project().join("d"))
        .unwrap()
        .is_dir());
    assert_eq!(read(&env.project().join("d/x.txt")), "base x\n");
    assert_eq!(read(&env.project().join("d/y.txt")), "base y\n");
}

#[test]
#[serial]
fn merge_refuses_to_replace_a_directory_the_base_added_to() {
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "base x\n");
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    write(&env.project().join("d/new.txt"), "base work\n");
    let view = env.view("w");
    fs::remove_file(view.join("d/x.txt")).unwrap();
    fs::remove_dir(view.join("d")).unwrap();
    symlink("elsewhere", view.join("d")).unwrap();

    let output = env.run(&["merge", "w", "--quiet", "--no-install"]);
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    assert_eq!(read(&env.project().join("d/new.txt")), "base work\n");
}

#[test]
#[serial]
fn pull_never_deletes_through_a_directory_the_base_replaced_with_a_symlink() {
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "base x\n");
    write(&env.project().join("d/y.txt"), "base y\n");
    write(&env.project().join("keep.txt"), "keep\n");
    sentinels(&env);
    env.fork("w");
    replace_dir_with_outside_link(&env, &env.project());

    env.ok(&["pull", "w", "--no-install"]);
    sentinels_intact(&env);
    assert_eq!(
        fs::read_link(env.view("w").join("d")).unwrap(),
        env.outside()
    );
}

#[test]
#[serial]
fn pull_never_writes_through_a_parent_swapped_for_a_symlink_after_planning() {
    skip_unless_copying!();
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "x v1\n");
    sentinels(&env);
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    write(&env.project().join("d/x.txt"), "x v2 from base\n");
    let workspace = env.workspace("w");
    let plan = pull::build(&workspace, false).unwrap();

    let view = &workspace.view_path;
    fs::rename(view.join("d"), view.join("d.real")).unwrap();
    symlink(env.outside(), view.join("d")).unwrap();
    let journal = pull::start(&workspace, &plan).unwrap();

    sentinels_intact(&env);
    assert_eq!(
        journal::held(&journal).unwrap(),
        vec![PathBuf::from("d/x.txt")]
    );
}

#[test]
#[serial]
fn pull_keeps_what_the_agent_writes_while_it_applies() {
    let env = Env::new();
    for (path, text) in [
        ("a.txt", "a0\n"),
        ("b.txt", "b0\n"),
        ("f.txt", "f0\n"),
        ("g/old.txt", "g0\n"),
    ] {
        write(&env.project().join(path), text);
    }
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    write(&env.project().join("a.txt"), "a1\n");
    write(&env.project().join("b.txt"), "b1 base\n");
    fs::remove_file(env.project().join("f.txt")).unwrap();
    fs::remove_dir_all(env.project().join("g")).unwrap();
    let workspace = env.workspace("w");
    let plan = pull::build(&workspace, false).unwrap();

    // The agent writes after planning, before the steps run.
    let view = &workspace.view_path;
    write(&view.join("b.txt"), "AGENT b\n");
    write(&view.join("f.txt"), "AGENT f\n");
    write(&view.join("g/new.txt"), "AGENT g\n");
    let journal = pull::start(&workspace, &plan).unwrap();

    assert_eq!(
        read(&view.join("a.txt")),
        "a1\n",
        "the base's edit is pulled"
    );
    assert_eq!(read(&view.join("b.txt")), "AGENT b\n");
    assert_eq!(read(&view.join("f.txt")), "AGENT f\n");
    assert_eq!(read(&view.join("g/new.txt")), "AGENT g\n");
    assert!(
        !view.join("g/old.txt").exists(),
        "the base's deletion still applies to what the agent did not write"
    );
    assert_eq!(
        journal::held(&journal).unwrap(),
        ["b.txt", "f.txt", "g/new.txt"].map(PathBuf::from).to_vec()
    );
}

#[test]
#[serial]
fn merge_leaves_base_edits_made_while_it_applies_and_keeps_the_workspace() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a0\n");
    write(&env.project().join("b.txt"), "b0\nline 2\n");
    write(&env.project().join("c.txt"), "c0\n");
    env.fork("w");
    let view = env.view("w");
    write(&view.join("a.txt"), "a1\n");
    write(&view.join("b.txt"), "b1 workspace\nline 2\n");
    fs::remove_file(view.join("c.txt")).unwrap();

    let Some(child) = env.spawn_slow(&["merge", "w", "--quiet", "--no-install", "--drop"], 400)
    else {
        return;
    };
    env.wait_for_journal("merge", 0);
    write(&env.project().join("b.txt"), "b0\nUSER EDIT\n");
    write(&env.project().join("c.txt"), "USER EDIT c\n");
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("b.txt") && stderr.contains("c.txt"),
        "{stderr}"
    );
    assert_eq!(read(&env.project().join("a.txt")), "a1\n");
    assert_eq!(read(&env.project().join("b.txt")), "b0\nUSER EDIT\n");
    assert_eq!(read(&env.project().join("c.txt")), "USER EDIT c\n");
    assert!(view.join("b.txt").exists(), "the workspace was dropped");

    env.ok(&["merge", "--undo", "w", "--no-install"]);
    assert_eq!(read(&env.project().join("a.txt")), "a0\n");
    assert_eq!(read(&env.project().join("b.txt")), "b0\nUSER EDIT\n");
    assert_eq!(read(&env.project().join("c.txt")), "USER EDIT c\n");
}

#[test]
#[serial]
fn a_resumed_merge_and_its_undo_leave_base_edits_made_while_it_was_interrupted() {
    let env = Env::new();
    for index in 1..=6 {
        write(&env.project().join(format!("f{index}.txt")), "base\n");
    }
    env.fork("w");
    for index in 1..=6 {
        write(
            &env.view("w").join(format!("f{index}.txt")),
            "base\nworkspace\n",
        );
    }
    let Some(mut child) = env.spawn_slow(&["merge", "w", "--quiet", "--no-install"], 300) else {
        return;
    };
    env.wait_for_journal("merge", 2);
    child.kill().unwrap();
    let _ = child.wait();
    for index in 1..=6 {
        let path = env.project().join(format!("f{index}.txt"));
        let text = read(&path);
        write(&path, &format!("{text}user {index}\n"));
    }

    let resumed = env.run(&["merge", "w", "--resume", "--no-install"]);
    assert_eq!(resumed.status.code(), Some(5), "{resumed:?}");
    for index in 1..=6 {
        let text = read(&env.project().join(format!("f{index}.txt")));
        assert!(
            text.ends_with(&format!("user {index}\n")),
            "f{index}: {text}"
        );
    }

    env.ok(&["merge", "--undo", "w", "--no-install"]);
    for index in 1..=6 {
        let text = read(&env.project().join(format!("f{index}.txt")));
        assert!(
            text.ends_with(&format!("user {index}\n")),
            "f{index}: {text}"
        );
    }
}

#[test]
#[serial]
fn a_resumed_pull_also_pulls_base_changes_made_while_it_was_interrupted() {
    let env = Env::new();
    for index in 1..=6 {
        write(&env.project().join(format!("f{index}.txt")), "base\n");
    }
    write(&env.project().join("a.txt"), "a0\n");
    write(&env.project().join("sub/s.txt"), "s0\n");
    env.fork("w");
    write(&env.view("w").join("sub/s.txt"), "s0\nagent\n");
    std::thread::sleep(Duration::from_millis(5));
    for index in 1..=6 {
        write(
            &env.project().join(format!("f{index}.txt")),
            "base\nbase edit\n",
        );
    }
    let Some(mut child) = env.spawn_slow(&["pull", "w", "--no-install"], 300) else {
        return;
    };
    env.wait_for_journal("pull", 2);
    child.kill().unwrap();
    let _ = child.wait();
    write(&env.project().join("late.txt"), "late\n");
    write(&env.project().join("a.txt"), "a0\nlate edit\n");

    env.ok(&["pull", "w", "--resume", "--no-install"]);
    let view = env.view("w");
    assert_eq!(read(&view.join("late.txt")), "late\n");
    assert_eq!(read(&view.join("a.txt")), "a0\nlate edit\n");
    for index in 1..=6 {
        assert_eq!(
            read(&view.join(format!("f{index}.txt"))),
            "base\nbase edit\n"
        );
    }

    env.ok(&["merge", "w", "--quiet", "--no-install"]);
    assert_eq!(read(&env.project().join("late.txt")), "late\n");
    assert_eq!(read(&env.project().join("a.txt")), "a0\nlate edit\n");
    assert_eq!(read(&env.project().join("sub/s.txt")), "s0\nagent\n");
}

#[test]
#[serial]
fn a_pull_also_pulls_base_changes_made_while_it_applies() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a0\n");
    write(&env.project().join("b.txt"), "b0\n");
    write(&env.project().join("d.txt"), "d0\n");
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    write(&env.project().join("a.txt"), "a1\n");
    write(&env.project().join("d.txt"), "d1\n");
    let Some(child) = env.spawn_slow(&["pull", "w", "--no-install"], 400) else {
        return;
    };
    // After the step for a.txt, before the one for d.txt.
    env.wait_for_journal("pull", 1);
    write(&env.project().join("a.txt"), "a2 during the pull\n");
    write(&env.project().join("b.txt"), "b1 during the pull\n");
    write(&env.project().join("c.txt"), "new during the pull\n");
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let view = env.view("w");
    // Pulled again over what the first pass installed, not merged with it.
    assert_eq!(read(&view.join("a.txt")), "a2 during the pull\n");
    assert_eq!(read(&view.join("b.txt")), "b1 during the pull\n");
    assert_eq!(read(&view.join("c.txt")), "new during the pull\n");
    assert_eq!(read(&view.join("d.txt")), "d1\n");
    let plan = plan::build(&env.workspace("w"), true).unwrap();
    assert!(plan.changes.is_empty(), "{:?}", plan.changes);
}

#[test]
#[serial]
fn merge_patch_drop_refuses_whenever_the_base_changed_a_patched_file() {
    let env = Env::new();
    let lines: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    write(&env.project().join("src/a.ts"), &lines);
    write(&env.project().join("other.txt"), "other\n");
    for (name, base_line) in [("overlap", "line 8\n"), ("apart", "line 18\n")] {
        env.fork(name);
        let view = env.view(name);
        write(
            &view.join("src/a.ts"),
            &lines.replace("line 8\n", "line 8 AGENT\n"),
        );
        write(&view.join("src/feature.ts"), "agent work\n");
        let edited = read(&env.project().join("src/a.ts")).replace(base_line, "BASE\n");
        write(&env.project().join("src/a.ts"), &edited);

        let output = env.run(&["merge", name, "--patch", "--drop"]);
        assert_eq!(output.status.code(), Some(5), "{name}: {output:?}");
        assert!(output.stdout.is_empty(), "{name}: a patch was printed");
        assert!(
            env.view(name).join("src/feature.ts").exists(),
            "{name} was dropped"
        );
        write(&env.project().join("src/a.ts"), &lines);
        env.ok(&["drop", name, "--force"]);
    }

    // A base change elsewhere does not stop the handoff.
    env.fork("clean");
    write(&env.view("clean").join("src/feature.ts"), "agent work\n");
    write(&env.project().join("other.txt"), "base edit\n");
    let output = env.ok(&["merge", "clean", "--patch", "--drop"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("src/feature.ts"));
}

#[test]
#[serial]
fn merge_on_its_own_still_overwrites_nothing_new() {
    // A plan applied right away holds nothing back.
    let env = Env::new();
    write(&env.project().join("a.txt"), "a0\n");
    env.fork("w");
    write(&env.view("w").join("a.txt"), "a1\n");
    let workspace = env.workspace("w");
    let result = merge::merge_workspace(&workspace).unwrap();
    assert_eq!(result.modified, 1);
    assert_eq!(read(&env.project().join("a.txt")), "a1\n");
}

#[test]
#[serial]
fn undo_never_restores_through_a_directory_replaced_by_a_symlink_since() {
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "base x\n");
    write(&env.project().join("d/keep.txt"), "keep\n");
    env.fork("w");
    fs::remove_file(env.view("w").join("d/x.txt")).unwrap();
    env.ok(&["merge", "w", "--quiet", "--no-install"]);
    assert!(!env.project().join("d/x.txt").exists());

    // After the merge, the base's directory becomes a link to one outside the project.
    fs::rename(env.project().join("d"), env.root.path().join("d.moved")).unwrap();
    symlink(env.outside(), env.project().join("d")).unwrap();
    env.ok(&["merge", "--undo", "w", "--no-install"]);
    assert!(
        files(&env.outside()).is_empty(),
        "undo wrote outside the project"
    );
}

#[test]
#[serial]
fn taking_one_side_of_a_path_both_sides_added_replaces_the_other() {
    let env = Env::new();
    write(&env.project().join("keep.txt"), "keep\n");
    env.fork("pulled");
    env.fork("merged");
    write(&env.project().join("both.bin"), "base\0bytes\n");
    write(&env.view("pulled").join("both.bin"), "agent\0bytes\n");
    write(&env.view("merged").join("both.bin"), "agent\0bytes\n");

    env.ok(&["pull", "pulled", "--theirs", "--no-install"]);
    assert_eq!(read(&env.view("pulled").join("both.bin")), "base\0bytes\n");

    env.ok(&[
        "merge",
        "merged",
        "--overwrite-base",
        "--quiet",
        "--no-install",
    ]);
    assert_eq!(read(&env.project().join("both.bin")), "agent\0bytes\n");
}

/// MERGE-19: `merge --drop` keeps the workspace when the agent writes in the view while the
/// merge runs: a new file, and an edit to a file the merge transfers.
#[test]
#[serial]
fn merge_drop_keeps_the_workspace_when_the_agent_writes_while_it_merges() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a0\n");
    write(&env.project().join("b.txt"), "b0\n");
    env.fork("w");
    let view = env.view("w");
    write(&view.join("a.txt"), "a1\n");
    write(&view.join("b.txt"), "b1\n");

    let Some(child) = env.spawn_slow(&["merge", "w", "--quiet", "--no-install", "--drop"], 400)
    else {
        return;
    };
    env.wait_for_journal("merge", 0);
    write(&view.join("late.txt"), "AGENT late\n");
    write(&view.join("a.txt"), "AGENT a2\n");
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("late.txt"), "{stderr}");
    assert_eq!(read(&env.view("w").join("late.txt")), "AGENT late\n");
    assert_eq!(read(&env.view("w").join("a.txt")), "AGENT a2\n");
    assert_eq!(read(&env.project().join("b.txt")), "b1\n");
}

/// The re-check finds nothing in a quiet merge: `--drop` still drops, also when the plan
/// keeps the base's lockfile.
#[test]
#[serial]
fn merge_drop_drops_a_workspace_nobody_wrote_to() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a0\n");
    write(&env.project().join("d/x.txt"), "x\n");
    env.fork("w");
    let view = env.view("w");
    write(&view.join("a.txt"), "a1\n");
    write(&view.join("new/deep/n.txt"), "n\n");
    fs::remove_dir_all(view.join("d")).unwrap();

    env.ok(&["merge", "w", "--quiet", "--no-install", "--drop"]);
    assert!(State::load().unwrap().workspaces.is_empty());
    assert_eq!(read(&env.project().join("new/deep/n.txt")), "n\n");
}

/// Move `notes` (a directory holding files) into `root/g`: the files keep their ctimes.
fn move_notes_into_g(root: &Path) {
    fs::rename(root.join("notes"), root.join("g/notes")).unwrap();
}

/// PULL-19: a directory the agent moves into one the pull deletes, after planning, stays with
/// its files; only what the base deleted goes.
#[test]
#[serial]
fn pull_keeps_a_directory_the_agent_moves_into_one_it_deletes() {
    let env = Env::new();
    write(&env.project().join("g/a.txt"), "g\n");
    write(&env.project().join("k.txt"), "k\n");
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    fs::remove_dir_all(env.project().join("g")).unwrap();
    let workspace = env.workspace("w");
    let view = &workspace.view_path;
    write(&view.join("notes/n1.md"), "IMPORTANT NOTES\n");
    write(&view.join("notes/n2.md"), "MORE\n");
    let plan = pull::build(&workspace, false).unwrap();

    move_notes_into_g(view);
    let journal = pull::start(&workspace, &plan).unwrap();
    assert_eq!(read(&view.join("g/notes/n1.md")), "IMPORTANT NOTES\n");
    assert_eq!(read(&view.join("g/notes/n2.md")), "MORE\n");
    assert!(
        !view.join("g/a.txt").exists(),
        "the base's deletion applies"
    );
    assert_eq!(
        journal::held(&journal).unwrap(),
        vec![PathBuf::from("g/notes")]
    );
}

/// PULL-19 on the merge side: the user moves a directory into one the merge deletes.
#[test]
#[serial]
fn merge_keeps_a_directory_moved_into_one_it_deletes() {
    let env = Env::new();
    write(&env.project().join("g/a.txt"), "g\n");
    write(&env.project().join("notes/n1.md"), "IMPORTANT NOTES\n");
    env.fork("w");
    fs::remove_dir_all(env.view("w").join("g")).unwrap();
    let workspace = env.workspace("w");
    let plan = plan::build(&workspace, false).unwrap();

    move_notes_into_g(&env.project());
    let journal = merge::execute(&plan).unwrap();
    assert_eq!(
        read(&env.project().join("g/notes/n1.md")),
        "IMPORTANT NOTES\n"
    );
    assert!(!env.project().join("g/a.txt").exists());
    assert_eq!(
        journal::held(&journal).unwrap(),
        vec![PathBuf::from("g/notes")]
    );
}

/// Re-baseline `workspace` after its pull journaled at `journal`, as `forkspace pull` does.
fn finish_pull(workspace: &mut Workspace, journal: &Path) -> Vec<PathBuf> {
    let left = pull::finish(workspace, journal).unwrap();
    State::with_state(|state| {
        *state.require_workspace_mut(&workspace.name)? = workspace.clone();
        state.save()
    })
    .unwrap();
    pull::complete(journal).unwrap();
    left
}

/// What `diff --files` lists, empty when it reports no changes.
fn diff_files(env: &Env, name: &str) -> String {
    let listed = String::from_utf8_lossy(&env.ok(&["diff", name, "--files"]).stdout).into_owned();
    if listed.starts_with("clean") {
        String::new()
    } else {
        listed
    }
}

/// PULL-23: base changes made after the pull last read the base (here: between its last step
/// and the re-baseline) are not in the new fork point: they stay base changes for the next
/// pull, never workspace reversions that the next merge would apply.
#[test]
#[serial]
fn base_changes_after_the_pull_last_read_the_base_are_left_for_the_next_pull() {
    skip_unless_copying!();
    let env = Env::new();
    for (path, text) in [
        ("a.txt", "a0\n"),
        ("c.txt", "c0\n"),
        ("d/x.txt", "x0\n"),
        ("p.txt", "p0\n"),
    ] {
        write(&env.project().join(path), text);
    }
    env.fork("w");
    write(&env.view("w").join("mine.txt"), "agent\n");
    std::thread::sleep(Duration::from_millis(5));
    write(&env.project().join("p.txt"), "p1\n");
    let mut workspace = env.workspace("w");
    let plan = pull::build(&workspace, false).unwrap();
    let journal = pull::start(&workspace, &plan).unwrap();

    write(&env.project().join("late.txt"), "late\n");
    write(&env.project().join("a.txt"), "a1 late\n");
    fs::remove_file(env.project().join("c.txt")).unwrap();
    fs::rename(env.project().join("d"), env.project().join("e")).unwrap();
    let mut left = finish_pull(&mut workspace, &journal);
    left.sort();
    assert_eq!(
        left,
        ["a.txt", "c.txt", "d/x.txt", "e/x.txt", "late.txt"].map(PathBuf::from)
    );
    assert_eq!(diff_files(&env, "w"), "mine.txt\n");

    env.ok(&["pull", "w", "--no-install"]);
    let view = env.view("w");
    assert_eq!(read(&view.join("late.txt")), "late\n");
    assert_eq!(read(&view.join("a.txt")), "a1 late\n");
    assert!(!view.join("c.txt").exists() && !view.join("d").exists());
    assert_eq!(read(&view.join("e/x.txt")), "x0\n");
    env.ok(&["merge", "w", "--quiet", "--no-install"]);
    assert_eq!(read(&env.project().join("late.txt")), "late\n");
    assert_eq!(read(&env.project().join("a.txt")), "a1 late\n");
    assert_eq!(read(&env.project().join("mine.txt")), "agent\n");
}

/// PULL-20: the agent writes a file the base deleted, so its step is held, and the base
/// changes something else during the pull: the catch-up round pulls that, never the held path
/// again, and the pull ends with no journal left.
#[test]
#[serial]
fn a_catch_up_round_never_wedges_on_what_the_agent_wrote() {
    let env = Env::new();
    for index in 1..=6 {
        write(&env.project().join(format!("f{index}.txt")), "f\n");
    }
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    fs::remove_file(env.project().join("f6.txt")).unwrap();
    for index in 1..=5 {
        write(&env.project().join(format!("f{index}.txt")), "planned\n");
    }
    let view = env.view("w");
    let Some(child) = env.spawn_slow(&["pull", "w", "--no-install"], 300) else {
        return;
    };
    env.wait_for_journal("pull", 0);
    write(&view.join("f6.txt"), "AGENT f6\n");
    write(&env.project().join("late.txt"), "late\n");
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(5), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("f6.txt"));
    assert_eq!(read(&view.join("f6.txt")), "AGENT f6\n");
    assert_eq!(read(&view.join("late.txt")), "late\n");
    let journals = fs::read_dir(env.home.path().join("journal"))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    assert_eq!(journals, 0, "a journal was left open");
    let resumed = env.run(&["pull", "w", "--resume", "--theirs", "--no-install"]);
    assert!(!resumed.status.success());
    assert_eq!(read(&view.join("f6.txt")), "AGENT f6\n");
}

/// PULL-21: a base directory renamed after its step ran: the round pulls the new name whole
/// (its entries keep their ctimes) and removes the old one, so view and base agree.
#[test]
#[serial]
fn a_catch_up_round_pulls_a_base_directory_renamed_during_the_pull() {
    skip_unless_copying!();
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "x0\n");
    write(&env.project().join("d/y.txt"), "y0\n");
    write(&env.project().join("z.txt"), "z0\n");
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    write(&env.project().join("d/x.txt"), "x1\n");
    write(&env.project().join("z.txt"), "z1\n");
    let mut workspace = env.workspace("w");
    let plan = pull::build(&workspace, false).unwrap();
    let journal = pull::start(&workspace, &plan).unwrap();

    fs::rename(env.project().join("d"), env.project().join("e")).unwrap();
    let previous = journal::load(&journal).unwrap();
    assert!(pull::base_moved_since(&workspace, previous.guard.as_ref()));
    let round = pull::build_since(&workspace, &previous, false).unwrap();
    let journal = pull::start_round(&workspace, &round, Some(&previous)).unwrap();
    assert_eq!(finish_pull(&mut workspace, &journal), Vec::<PathBuf>::new());

    assert_eq!(files(&env.view("w")), files(&env.project()));
    assert_eq!(diff_files(&env, "w"), "");
}

/// PULL-22: a base file the plan saw that is gone before its step is a base deletion, not an
/// error that wedges the pull.
#[test]
#[serial]
fn a_base_file_gone_before_its_step_is_a_base_deletion() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a0\n");
    write(&env.project().join("gone.txt"), "g0\n");
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    write(&env.project().join("a.txt"), "a1\n");
    write(&env.project().join("gone.txt"), "g1\n");
    write(&env.project().join("swap.tmp"), "editor swap\n");
    let workspace = env.workspace("w");
    let plan = pull::build(&workspace, false).unwrap();

    fs::remove_file(env.project().join("gone.txt")).unwrap();
    fs::remove_file(env.project().join("swap.tmp")).unwrap();
    let journal = pull::start(&workspace, &plan).unwrap();
    let view = env.view("w");
    assert_eq!(read(&view.join("a.txt")), "a1\n");
    assert!(!view.join("gone.txt").exists());
    assert!(!view.join("swap.tmp").exists());
    assert!(journal::held(&journal).unwrap().is_empty());
}

/// PULL-22: a base file rewritten every few milliseconds for the whole pull: the pull ends
/// (exit 0) with the file left for the next pull, and nothing looks like a workspace change.
#[test]
#[serial]
fn a_base_file_that_keeps_changing_is_left_for_the_next_pull() {
    skip_unless_copying!();
    let env = Env::new();
    for index in 0..8 {
        write(&env.project().join(format!("src/f{index}.txt")), "v0\n");
    }
    write(&env.project().join("hot.txt"), "0\n");
    env.fork("w");
    std::thread::sleep(Duration::from_millis(5));
    for index in 0..8 {
        write(&env.project().join(format!("src/f{index}.txt")), "v1\n");
    }
    let Some(child) = env.spawn_slow(&["pull", "w", "--no-install"], 50) else {
        return;
    };
    let hot = env.project().join("hot.txt");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut count = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                count += 1;
                let temporary = hot.with_extension("tmp");
                fs::write(&temporary, format!("{count}\n")).unwrap();
                fs::rename(&temporary, &hot).unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let output = child.wait_with_output().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(diff_files(&env, "w"), "");
    env.ok(&["pull", "w", "--no-install"]);
    assert_eq!(
        read(&env.view("w").join("hot.txt")),
        read(&env.project().join("hot.txt"))
    );
    assert_eq!(diff_files(&env, "w"), "");
}

/// DIFF-12: a base file whose bytes went back to the fork's with a new mtime is not pulled,
/// and after the pull it is no change of the workspace's either.
#[test]
#[serial]
fn a_base_file_touched_back_to_its_bytes_is_no_change_after_a_pull() {
    let env = Env::new();
    write(&env.project().join("m1.ts"), "m1\n");
    write(&env.project().join("m2.ts"), "m2\n");
    write(&env.project().join("m3.ts"), "m3\n");
    env.fork("w");
    write(&env.view("w").join("m1.ts"), "AGENT\n");
    std::thread::sleep(Duration::from_millis(20));
    write(&env.project().join("m2.ts"), "m2\n");
    write(&env.project().join("m3.ts"), "m3 base\n");

    env.ok(&["pull", "w", "--no-install"]);
    assert_eq!(read(&env.view("w").join("m3.ts")), "m3 base\n");
    assert_eq!(diff_files(&env, "w"), "m1.ts\n");
}

/// MERGE-36: a file both sides changed the same way needs nothing from the patch:
/// `merge --patch --drop` leaves it out, the patch applies, and the workspace is dropped.
#[test]
#[serial]
fn merge_patch_drop_leaves_out_what_the_base_already_has() {
    let env = Env::new();
    let lines: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    write(&env.project().join("a.txt"), &lines);
    env.fork("w");
    let same = lines.replace("line 8\n", "line 8 SAME\n");
    write(&env.view("w").join("a.txt"), &same);
    write(&env.view("w").join("new.txt"), "new\n");
    write(&env.project().join("a.txt"), &same);

    let output = env.ok(&["merge", "w", "--patch", "--drop"]);
    let patch = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(patch.contains("new.txt"), "{patch}");
    assert!(!patch.contains("a.txt"), "{patch}");
    assert!(
        env.run(&["diff", "w"]).status.code() != Some(0),
        "w was kept"
    );
    let mut apply = Command::new("git")
        .args(["apply", "-"])
        .current_dir(env.project())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    apply
        .stdin
        .take()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    assert!(apply.wait().unwrap().success());
    assert_eq!(read(&env.project().join("new.txt")), "new\n");
    assert_eq!(read(&env.project().join("a.txt")), same);
}
