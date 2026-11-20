//! Steps that change what kind of entry a path holds (a file, a directory, a symlink) replace
//! the old entry whole, and undo and restore turn it back: UNDO-01, UNDO-02, CKPT-09, CKPT-10,
//! FS-10, PULL-16 and MERGE-28.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use forkspace::state::State;
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
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn view(&self, name: &str) -> PathBuf {
        State::load()
            .unwrap()
            .require_workspace(name)
            .unwrap()
            .view_path
            .clone()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", "copy")
            .args(args)
            .output()
            .unwrap()
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
        let project = self.project();
        self.ok(&["new", name, "--from", project.to_str().unwrap(), "--copy"]);
    }

    /// The journals still open in the store.
    fn open_journals(&self) -> Vec<String> {
        fs::read_dir(self.home.path().join("journal"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.ends_with(".json"))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("<{error}>"))
}

/// Let the clock move past the fork, so a base write after it shows as a base change.
fn after_fork() {
    std::thread::sleep(Duration::from_millis(5));
}

/// Every entry under \`root\` but \`.git\`, with its kind and content or link target, sorted.
fn tree(root: &Path) -> Vec<(PathBuf, String)> {
    let mut found: Vec<_> = walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .flatten()
        .map(|entry| {
            let kind = if entry.path_is_symlink() {
                format!("link {}", fs::read_link(entry.path()).unwrap().display())
            } else if entry.file_type().is_dir() {
                "dir".to_string()
            } else {
                format!("file {}", read(entry.path()))
            };
            (entry.path().strip_prefix(root).unwrap().to_path_buf(), kind)
        })
        .collect();
    found.sort();
    found
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// UNDO-01 and UNDO-02: one merge turns a directory into a file and a file into a directory.
#[test]
#[serial]
fn merge_undo_turns_a_file_made_a_directory_and_a_directory_made_a_file_back() {
    let env = Env::new();
    write(&env.project().join("dir2file/x.txt"), "x\n");
    write(&env.project().join("file2dir"), "ORIGINAL BASE FILE\n");
    write(&env.project().join("keep.txt"), "keep\n");
    env.fork("w");
    let view = env.view("w");
    fs::remove_dir_all(view.join("dir2file")).unwrap();
    write(&view.join("dir2file"), "now a file\n");
    fs::remove_file(view.join("file2dir")).unwrap();
    write(&view.join("file2dir/in.txt"), "inside\n");
    write(&view.join("keep.txt"), "keep 2\n");
    let before = tree(&env.project());

    env.ok(&["merge", "w", "--quiet", "--no-install"]);
    assert_eq!(read(&env.project().join("dir2file")), "now a file\n");
    assert_eq!(read(&env.project().join("file2dir/in.txt")), "inside\n");

    env.ok(&["merge", "--undo", "w", "--no-install"]);
    assert_eq!(tree(&env.project()), before);
}

/// UNDO-02: a directory the merge put where a file was, written into since, is left alone and
/// the file stays in the undo record.
#[test]
#[serial]
fn merge_undo_leaves_a_directory_written_into_since_the_merge() {
    let env = Env::new();
    write(&env.project().join("file2dir"), "ORIGINAL BASE FILE\n");
    env.fork("w");
    let view = env.view("w");
    fs::remove_file(view.join("file2dir")).unwrap();
    write(&view.join("file2dir/in.txt"), "inside\n");
    env.ok(&["merge", "w", "--quiet", "--no-install"]);
    write(&env.project().join("file2dir/later.txt"), "user\n");

    let output = env.ok(&["merge", "--undo", "w", "--no-install"]);
    assert_eq!(
        read(&env.project().join("file2dir/later.txt")),
        "user\n",
        "{}",
        stderr(&output)
    );
    let kept: Vec<_> = walkdir::WalkDir::new(env.home.path().join("undo"))
        .into_iter()
        .flatten()
        .filter(|entry| entry.file_type().is_file() && read(entry.path()) == "ORIGINAL BASE FILE\n")
        .collect();
    assert_eq!(kept.len(), 1, "the base's file is still in the undo record");
}

/// CKPT-09: a directory and a symlink deleted after a checkpoint come back with \`restore\`.
#[test]
#[serial]
fn restore_puts_back_a_deleted_directory_and_symlink() {
    let env = Env::new();
    write(&env.project().join("h/a"), "alpha\n");
    write(&env.project().join("h/sub/b"), "beta\n");
    write(&env.project().join("t"), "target\n");
    symlink("t", env.project().join("l")).unwrap();
    env.fork("w");
    let view = env.view("w");
    env.ok(&["checkpoint", "w", "-m", "clean"]);
    let saved = tree(&view);
    fs::remove_dir_all(view.join("h")).unwrap();
    fs::remove_file(view.join("l")).unwrap();

    let output = env.ok(&["restore", "w", "1", "--no-install"]);
    if cfg!(target_os = "macos") {
        assert_eq!(tree(&view), saved, "{}", stderr(&output));
        assert!(
            !stderr(&output).contains("left as they are"),
            "{}",
            stderr(&output)
        );
    } else {
        // Without a clone the fork-time copy keeps regular files only: the directory comes
        // back, and the symlink is named as left alone rather than silently missing.
        let without_link: Vec<_> = saved
            .into_iter()
            .filter(|(path, _)| path != Path::new("l"))
            .collect();
        assert_eq!(tree(&view), without_link, "{}", stderr(&output));
        assert!(
            stderr(&output)
                .contains("left as they are because their fork-time version is unknown: l"),
            "{}",
            stderr(&output)
        );
    }
}

/// FS-10: restore swaps a directory for the symlink the checkpoint saved, and back.
#[test]
#[serial]
fn restore_swaps_a_directory_and_a_symlink_both_ways() {
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "base x\n");
    write(&env.project().join("d/y.txt"), "base y\n");
    write(&env.project().join("keep.txt"), "keep\n");
    env.fork("w");
    let view = env.view("w");
    fs::remove_dir_all(view.join("d")).unwrap();
    symlink("elsewhere", view.join("d")).unwrap();
    env.ok(&["checkpoint", "w", "-m", "link"]);
    let as_link = tree(&view);
    fs::remove_file(view.join("d")).unwrap();
    write(&view.join("d/x.txt"), "view x\n");
    let as_dir = tree(&view);

    // Checkpoint 2 is the automatic one taken before this restore: the directory.
    env.ok(&["restore", "w", "1", "--no-install"]);
    assert_eq!(tree(&view), as_link);
    env.ok(&["restore", "w", "2", "--no-install"]);
    assert_eq!(tree(&view), as_dir);
}

/// CKPT-10: after \`pull --theirs\` takes a file the base put where a directory was, the
/// before-pull checkpoint still brings back what the workspace added in the directory.
#[test]
#[serial]
fn the_before_pull_checkpoint_restores_after_the_base_turned_a_directory_into_a_file() {
    let env = Env::new();
    write(&env.project().join("dir/keep.txt"), "keep\n");
    write(&env.project().join("dir/sub/k2.txt"), "k2\n");
    write(&env.project().join("top.txt"), "top\n");
    env.fork("w");
    let view = env.view("w");
    write(&view.join("dir/new.txt"), "new\n");
    write(&view.join("dir/sub/new2.txt"), "new2\n");
    after_fork();
    fs::remove_dir_all(env.project().join("dir")).unwrap();
    write(&env.project().join("dir"), "now a file\n");
    env.ok(&["pull", "w", "--theirs", "--no-install"]);
    assert_eq!(read(&view.join("dir")), "now a file\n");

    let output = env.ok(&["restore", "w", "1", "--no-install"]);
    assert_eq!(
        read(&view.join("dir/new.txt")),
        "new\n",
        "{}",
        stderr(&output)
    );
    assert_eq!(read(&view.join("dir/sub/new2.txt")), "new2\n");
    assert_eq!(env.open_journals(), Vec::<String>::new());
}

/// PULL-16: the workspace replaced a directory with a file, and the base added a file inside
/// the directory: a conflict, found before anything is applied; \`--theirs\` takes the base's
/// directory whole.
#[test]
#[serial]
fn pull_conflicts_when_the_base_adds_inside_a_directory_the_workspace_replaced() {
    let env = Env::new();
    write(&env.project().join("e/h/keep"), "k\n");
    write(&env.project().join("y"), "y\n");
    env.fork("w");
    let view = env.view("w");
    fs::remove_dir_all(view.join("e/h")).unwrap();
    write(&view.join("e/h"), "a file\n");
    after_fork();
    write(&env.project().join("e/h/a.md"), "new\n");
    write(&env.project().join("y"), "y2\n");

    let output = env.run(&["pull", "w", "--no-install"]);
    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
    assert!(stderr(&output).contains("e/h"), "{}", stderr(&output));
    assert_eq!(read(&view.join("e/h")), "a file\n");
    assert_eq!(read(&view.join("y")), "y\n", "nothing is applied");
    assert_eq!(env.open_journals(), Vec::<String>::new());

    env.ok(&["pull", "w", "--theirs", "--no-install"]);
    assert_eq!(tree(&view), tree(&env.project()));
}

/// PULL-24: the base turned a fork-time file into a directory (or a symlink) the workspace
/// never touched: pull takes it, as for any other base change.
#[test]
#[serial]
fn pull_takes_a_file_the_base_turned_into_a_directory() {
    for shape in ["nested", "flat", "symlink"] {
        let env = Env::new();
        write(&env.project().join("dir"), "a file\n");
        write(&env.project().join("top.txt"), "top\n");
        env.fork("w");
        after_fork();
        fs::remove_file(env.project().join("dir")).unwrap();
        match shape {
            "nested" => write(&env.project().join("dir/x/y"), "back\n"),
            "flat" => write(&env.project().join("dir/y"), "back\n"),
            _ => symlink("top.txt", env.project().join("dir")).unwrap(),
        }

        let output = env.run(&["pull", "w", "--no-install"]);
        assert!(output.status.success(), "{shape}: {}", stderr(&output));
        assert_eq!(tree(&env.view("w")), tree(&env.project()), "{shape}");
        assert_eq!(env.open_journals(), Vec::<String>::new(), "{shape}");
    }
}

/// PULL-24's other side: the workspace edited the file the base turned into a directory, so
/// both changed it: a conflict on the entry, nothing applied.
#[test]
#[serial]
fn pull_conflicts_when_the_workspace_edited_a_file_the_base_turned_into_a_directory() {
    let env = Env::new();
    write(&env.project().join("dir"), "a file\n");
    env.fork("w");
    let view = env.view("w");
    write(&view.join("dir"), "the workspace's edit\n");
    after_fork();
    fs::remove_file(env.project().join("dir")).unwrap();
    write(&env.project().join("dir/y"), "back\n");

    let output = env.run(&["pull", "w", "--no-install"]);
    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
    assert!(stderr(&output).contains("dir"), "{}", stderr(&output));
    assert_eq!(read(&view.join("dir")), "the workspace's edit\n");
}

/// CKPT-11: a checkpoint taken after the workspace deleted a fork-time directory restores
/// that deletion whole, whatever the path holds since; nothing comes back from the fork.
#[test]
#[serial]
fn restore_keeps_a_directory_deleted_at_the_checkpoint_deleted() {
    for since in ["dir with other entries", "flat", "file"] {
        let env = Env::new();
        write(&env.project().join("a/b.txt"), "fork time\n");
        write(&env.project().join("k"), "k\n");
        env.fork("w");
        let view = env.view("w");
        fs::remove_dir_all(view.join("a")).unwrap();
        env.ok(&["checkpoint", "w", "-m", "deleted"]);
        let saved = tree(&view);
        match since {
            "dir with other entries" => write(&view.join("a/e/f.md"), "new\n"),
            "flat" => write(&view.join("a/other.txt"), "new\n"),
            _ => write(&view.join("a"), "now a file\n"),
        }

        let output = env.ok(&["restore", "w", "1", "--no-install"]);
        assert_eq!(tree(&view), saved, "{since}: {}", stderr(&output));
    }
}

/// MERGE-38: the workspace replaced a symlink to a directory with a real directory holding a
/// file of a name the linked directory also has. The base's path through its symlink is not
/// the same path: the merge replaces the symlink and adds the file, with no conflict.
#[test]
#[serial]
fn merge_replaces_a_symlinked_directory_with_the_workspace_directory() {
    let env = Env::new();
    write(&env.project().join("c/d.txt"), "eps\nbeta\ndelta");
    symlink("c", env.project().join("b")).unwrap();
    env.fork("w");
    let view = env.view("w");
    fs::remove_file(view.join("b")).unwrap();
    write(&view.join("b/d.txt"), "theta\ntheta\n");

    let output = env.run(&["merge", "w", "--quiet", "--no-install"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(tree(&env.project()), tree(&view));
    assert_eq!(read(&env.project().join("c/d.txt")), "eps\nbeta\ndelta");
    env.ok(&["merge", "--undo", "w", "--no-install"]);
    assert_eq!(
        fs::read_link(env.project().join("b")).unwrap(),
        Path::new("c")
    );
}

/// MERGE-35: `merge --patch --drop` where the base has a file or symlink where the patch needs
/// a directory refuses before printing, as a direct merge does, and keeps the workspace.
#[test]
#[serial]
fn merge_patch_drop_refuses_when_the_base_has_a_file_where_the_patch_needs_a_directory() {
    for blocking in ["file", "symlink"] {
        let env = Env::new();
        write(&env.project().join("k.txt"), "k\n");
        write(&env.project().join("ex/e.txt"), "e\n");
        env.fork("w");
        let view = env.view("w");
        write(&view.join("ex/new.txt"), "agent work\n");
        after_fork();
        fs::remove_dir_all(env.project().join("ex")).unwrap();
        if blocking == "file" {
            write(&env.project().join("ex"), "base file\n");
        } else {
            symlink(env.root.path(), env.project().join("ex")).unwrap();
        }

        let output = env.run(&["merge", "w", "--patch", "--drop"]);
        assert_eq!(
            output.status.code(),
            Some(5),
            "{blocking}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{blocking}: a patch was printed");
        assert_eq!(read(&view.join("ex/new.txt")), "agent work\n", "{blocking}");
    }
}

/// MERGE-28: the base put a file where the workspace added a directory: a conflict before
/// anything is applied; \`--overwrite-base\` replaces the file, and undo puts it back.
#[test]
#[serial]
fn merge_conflicts_when_the_base_put_a_file_where_the_workspace_added_a_directory() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork("w");
    let view = env.view("w");
    write(&view.join("a.txt"), "a workspace\n");
    write(&view.join("d/y"), "workspace dir\n");
    after_fork();
    write(&env.project().join("d"), "base file named d\n");
    let before = tree(&env.project());

    let output = env.run(&["merge", "w", "--quiet", "--no-install"]);
    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
    assert!(stderr(&output).contains("d/y"), "{}", stderr(&output));
    assert_eq!(tree(&env.project()), before, "nothing is applied");
    assert_eq!(env.open_journals(), Vec::<String>::new());

    env.ok(&["merge", "w", "--quiet", "--no-install", "--overwrite-base"]);
    assert_eq!(read(&env.project().join("d/y")), "workspace dir\n");
    assert_eq!(read(&env.project().join("a.txt")), "a workspace\n");
    env.ok(&["merge", "--undo", "w", "--no-install"]);
    assert_eq!(tree(&env.project()), before);
}

/// Whether \`dir\` is on a volume that resolves names case-insensitively (APFS by default).
fn case_insensitive(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe");
    fs::write(&probe, "probe\n").unwrap();
    let insensitive = dir.join("caseprobe").exists();
    fs::remove_file(probe).unwrap();
    insensitive
}

/// Rename \`from\` to \`to\` in \`root\` when the two differ only in case.
fn respell(root: &Path, from: &str, to: &str) {
    fs::rename(root.join(from), root.join("respelling")).unwrap();
    fs::rename(root.join("respelling"), root.join(to)).unwrap();
}

/// The names directly in \`root\`, spelled as listed.
fn names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != ".git")
        .collect();
    names.sort();
    names
}

/// PULL-17: the base renames \`dir\` to \`Dir\` (one entry on a case-insensitive volume) while
/// the workspace added or edited something inside: a conflict, found before anything is
/// applied; \`--theirs\` takes the base's spelling and contents.
#[test]
#[serial]
fn pull_conflicts_when_the_base_respells_a_directory_the_workspace_changed_inside() {
    let env = Env::new();
    if !case_insensitive(&env.project()) {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"case-insensitive-volume\",\"reason\":\"test volume is case-sensitive\"}}"
        );
        return;
    }
    write(&env.project().join("dir/keep.txt"), "keep\n");
    write(&env.project().join("dir/sub/k2.txt"), "k2\n");
    write(&env.project().join("other.txt"), "o\n");
    env.fork("w");
    env.fork("edit");
    let view = env.view("w");
    write(&view.join("dir/new.txt"), "workspace addition\n");
    let edit_view = env.view("edit");
    write(&edit_view.join("dir/keep.txt"), "keep\nworkspace edit\n");
    after_fork();
    respell(&env.project(), "dir", "Dir");

    for (name, path, text) in [
        ("w", "dir/new.txt", "workspace addition\n"),
        ("edit", "dir/keep.txt", "keep\nworkspace edit\n"),
    ] {
        let view = env.view(name);
        let output = env.run(&["pull", name, "--no-install"]);
        assert_eq!(output.status.code(), Some(5), "{name}: {}", stderr(&output));
        assert!(stderr(&output).contains("dir"), "{}", stderr(&output));
        assert_eq!(read(&view.join(path)), text, "{name}");
        assert_eq!(
            names(&view),
            ["dir", "other.txt"],
            "{name}: nothing is applied"
        );
        assert_eq!(env.open_journals(), Vec::<String>::new());
    }

    env.ok(&["pull", "w", "--theirs", "--no-install"]);
    assert_eq!(names(&view), ["Dir", "other.txt"]);
    assert_eq!(tree(&view), tree(&env.project()));
}

/// PULL-18: on an overlay view, a pull that turns a directory of the lower layer into a file
/// removes the old tree in place; fuse-overlayfs cannot remove a directory swapped away.
#[test]
#[serial]
fn pull_replaces_a_lower_layer_directory_with_a_file_on_an_overlay_view() {
    skip_without_overlay!();
    let env = Env::new();
    write(&env.project().join("dir/keep.txt"), "keep\n");
    write(&env.project().join("dir/sub/k2.txt"), "k2\n");
    write(&env.project().join("top.txt"), "top\n");
    let overlay = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", env.home.path())
            .env_remove("FORKSPACE_BACKEND")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    };
    overlay(&["new", "w", "--from", env.project().to_str().unwrap()]);
    let view = env.view("w");
    after_fork();
    fs::remove_dir_all(env.project().join("dir")).unwrap();
    write(&env.project().join("dir"), "now a file\n");

    overlay(&["pull", "w", "--no-install"]);
    assert_eq!(read(&view.join("dir")), "now a file\n");
    assert_eq!(names(&view), ["dir", "top.txt"]);
    assert_eq!(env.open_journals(), Vec::<String>::new());
    overlay(&["drop", "w"]);
}

/// FS-10, the other way: a directory the checkpoint saved, now a symlink to a directory
/// outside the view holding entries of the same names. The restore puts a directory back and
/// never writes or reads through the symlink.
#[test]
#[serial]
fn restore_replaces_a_symlinked_directory_without_touching_its_target() {
    let env = Env::new();
    write(&env.project().join("d/x.txt"), "base x\n");
    write(&env.project().join("d/y.txt"), "base y\n");
    let outside = env.root.path().join("outside");
    write(&outside.join("x.txt"), "SENTINEL x\n");
    write(&outside.join("y.txt"), "SENTINEL y\n");
    env.fork("w");
    let view = env.view("w");
    write(&view.join("d/x.txt"), "checkpointed x\n");
    fs::remove_file(view.join("d/y.txt")).unwrap();
    env.ok(&["checkpoint", "w", "-m", "dir"]);
    let saved = tree(&view);
    fs::remove_dir_all(view.join("d")).unwrap();
    symlink(&outside, view.join("d")).unwrap();

    let output = env.ok(&["restore", "w", "1", "--no-install"]);
    assert_eq!(tree(&view), saved, "{}", stderr(&output));
    assert_eq!(read(&outside.join("x.txt")), "SENTINEL x\n");
    assert_eq!(read(&outside.join("y.txt")), "SENTINEL y\n");
}
