//! `merge --patch` exports: every change reaches the patch byte for byte, or nothing is
//! printed. Each test applies the patch with `git apply` to a copy of the base and compares the
//! result with the workspace view, so a patch that silently leaves a change out fails here.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use forkspace::backend::{BackendKind, BackendState};
use forkspace::changes::manifest::{self, ManifestStatus};
use forkspace::changes::{collect_clone_changes, FileChange, OldContent};
use forkspace::error::ForkspaceError;
use forkspace::sync::{ancestor, merge};
use forkspace::util::now_unix_ns;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn git_available() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("FORKSPACE_TEST_SKIP {{\"capability\":\"git\",\"reason\":\"git apply checks the patch\"}}");
    }
    found
}

/// A snapshot workspace over `view` (an exact copy of `base`), forked now, with its fork-time
/// copy recorded the way `new` records it. The manifest is left to the caller, so a test can
/// build it after an early edit, as the background walk would.
fn fork(base: &Path, view: &Path, name: &str) -> Workspace {
    copy_tree(base, view);
    let mut workspace = Workspace::new(name, base.to_path_buf(), None);
    workspace.backend_kind = BackendKind::MacClone;
    workspace.backend_state = BackendState::MacClone {
        unknown_fields: serde_json::Map::new(),
    };
    workspace.view_path = view.to_path_buf();
    std::thread::sleep(std::time::Duration::from_millis(5));
    workspace.fork_time_ns = now_unix_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    ancestor::record(name, view).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    workspace
}

fn build_manifest(workspace: &Workspace) {
    manifest::build(
        &workspace.name,
        &workspace.view_path,
        workspace.fork_time_ns,
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
}

fn changes(workspace: &Workspace) -> Vec<FileChange> {
    collect_clone_changes(workspace, true, false).unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    let status = Command::new("cp")
        .arg("-Rp")
        .arg(format!("{}/.", from.display()))
        .arg(to)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Apply `patch` to a copy of `base` and return that copy.
fn apply(base: &Path, patch: &[u8]) -> tempfile::TempDir {
    let copy = tempfile::tempdir().unwrap();
    copy_tree(base, copy.path());
    let patch_file = copy.path().join(".forkspace-test.patch");
    fs::write(&patch_file, patch).unwrap();
    for args in [
        &["init", "-q"][..],
        &["apply", "--check", ".forkspace-test.patch"],
        &["apply", ".forkspace-test.patch"],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(copy.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}\npatch:\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(patch)
        );
    }
    fs::remove_file(patch_file).unwrap();
    fs::remove_dir_all(copy.path().join(".git")).unwrap();
    copy
}

#[derive(Debug, PartialEq)]
enum Node {
    File { bytes: Vec<u8>, executable: bool },
    Symlink(PathBuf),
}

/// Every file and symlink under `root` (directories are implied by their entries).
fn tree(root: &Path) -> BTreeMap<PathBuf, Node> {
    walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| !entry.file_type().is_dir())
        .map(|entry| {
            let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
            let node = if entry.file_type().is_symlink() {
                Node::Symlink(fs::read_link(entry.path()).unwrap())
            } else {
                Node::File {
                    bytes: fs::read(entry.path()).unwrap(),
                    executable: entry.metadata().unwrap().permissions().mode() & 0o111 != 0,
                }
            };
            (relative, node)
        })
        .collect()
}

fn assert_same_tree(applied: &Path, view: &Path) {
    let (applied, view) = (tree(applied), tree(view));
    let differing: Vec<_> = applied
        .keys()
        .chain(view.keys())
        .filter(|path| applied.get(*path) != view.get(*path))
        .collect();
    assert!(
        differing.is_empty(),
        "the applied patch differs from the workspace at {differing:?}"
    );
}

/// Catches the batch 112 export: files written before the manifest walk reached them have dirty
/// rows, and their diffs were replaced by "diff unavailable" lines that `git apply` ignores.
#[test]
#[serial]
fn files_written_before_the_manifest_walk_are_exported_from_the_fork_time_copy() {
    if !git_available() {
        return;
    }
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    for name in ["early.rs", "late.rs", "untouched.rs"] {
        fs::write(
            base.path().join(name),
            format!("fn {}() {{}}\n// end\n", &name[..4]),
        )
        .unwrap();
    }
    let workspace = fork(base.path(), view.path(), "early-writes");
    fs::write(
        view.path().join("early.rs"),
        "fn early() { changed() }\n// end\n",
    )
    .unwrap();
    build_manifest(&workspace);
    fs::write(
        view.path().join("late.rs"),
        "fn late() { changed() }\n// end\n",
    )
    .unwrap();

    let changes = changes(&workspace);
    let early = changes
        .iter()
        .find(|change| change.path == Path::new("early.rs"))
        .unwrap();
    assert_eq!(
        early.old_content,
        OldContent::AncestorUnknown,
        "the early write should leave a dirty row"
    );

    let patch = merge::generate_patch(&workspace, view.path(), &changes).unwrap();
    assert!(!String::from_utf8_lossy(&patch).contains("unavailable"));
    let applied = apply(base.path(), &patch);
    assert_same_tree(applied.path(), view.path());
}

/// When neither the base nor a fork-time copy holds the old version, the export refuses and
/// names the file instead of printing a patch without it.
#[test]
#[serial]
fn a_change_with_no_known_fork_time_version_refuses_the_whole_patch() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("early.rs"), "old\n").unwrap();
    fs::write(base.path().join("late.rs"), "old\n").unwrap();
    let workspace = fork(base.path(), view.path(), "no-ancestor");
    fs::write(view.path().join("early.rs"), "new\n").unwrap();
    build_manifest(&workspace);
    fs::write(view.path().join("late.rs"), "new\n").unwrap();
    fs::remove_file(ancestor::dir("no-ancestor").join("early.rs")).unwrap();

    let error = merge::generate_patch(&workspace, view.path(), &changes(&workspace)).unwrap_err();
    let ForkspaceError::PatchIncomplete { omitted, .. } = &error else {
        panic!("expected PatchIncomplete, got {error:?}");
    };
    assert_eq!(omitted.len(), 1, "{omitted:?}");
    assert!(omitted[0].starts_with("early.rs ("), "{omitted:?}");
    assert_eq!(error.exit_code(), 5);
}

/// Each case here was wrong in the 0.2.0 exporter: line ends, final newlines, non-UTF-8 text,
/// executable bits, symlinks, empty files, deleted directories and quoted names.
#[test]
#[serial]
fn the_patch_reproduces_bytes_modes_symlinks_and_deletions() {
    if !git_available() {
        return;
    }
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    let b = base.path();
    fs::create_dir_all(b.join("gone/deep")).unwrap();
    fs::write(b.join("gone/one"), "1\n").unwrap();
    fs::write(b.join("gone/deep/two"), "2\n").unwrap();
    fs::write(b.join("crlf.txt"), "a\r\nb\r\n").unwrap();
    fs::write(b.join("no-final-newline.txt"), "old\n").unwrap();
    fs::write(b.join("latin1.txt"), "keep\n").unwrap();
    fs::write(b.join("script.sh"), "echo hi\n").unwrap();
    fs::write(b.join("emptied.txt"), "text\n").unwrap();
    fs::write(b.join("becomes-link"), "file\n").unwrap();
    fs::write(b.join("deleted-empty"), "").unwrap();
    let mut long: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    fs::write(b.join("long.txt"), &long).unwrap();

    let workspace = fork(b, view.path(), "fidelity");
    build_manifest(&workspace);
    let v = view.path();
    fs::remove_dir_all(v.join("gone")).unwrap();
    fs::write(v.join("crlf.txt"), "a\r\nB\r\n").unwrap();
    fs::write(v.join("no-final-newline.txt"), "new").unwrap();
    fs::write(v.join("latin1.txt"), b"caf\xe9\n").unwrap();
    fs::set_permissions(v.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(v.join("emptied.txt"), "").unwrap();
    fs::remove_file(v.join("becomes-link")).unwrap();
    symlink("script.sh", v.join("becomes-link")).unwrap();
    fs::remove_file(v.join("deleted-empty")).unwrap();
    symlink("long.txt", v.join("new-link")).unwrap();
    fs::write(v.join("new-empty"), "").unwrap();
    fs::write(v.join("added-no-newline"), "tail").unwrap();
    fs::create_dir_all(v.join("new/dir")).unwrap();
    fs::write(v.join("new/dir/file"), "deep\n").unwrap();
    fs::write(v.join("with space.txt"), "space\n").unwrap();
    fs::write(v.join("caf\u{e9}.txt"), "accent\n").unwrap();
    long = long
        .replace("line 5\n", "LINE 5\n")
        .replace("line 35\n", "");
    fs::write(v.join("long.txt"), &long).unwrap();

    let patch = merge::generate_patch(&workspace, v, &changes(&workspace)).unwrap();
    let applied = apply(b, &patch);
    assert_same_tree(applied.path(), v);
}

#[test]
#[serial]
fn binary_content_refuses_the_patch() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("blob"), b"\0old").unwrap();
    fs::write(base.path().join("text"), "old\n").unwrap();
    let workspace = fork(base.path(), view.path(), "binary");
    build_manifest(&workspace);
    fs::write(view.path().join("blob"), b"\0new").unwrap();
    fs::write(view.path().join("text"), "new\n").unwrap();

    let error = merge::generate_patch(&workspace, view.path(), &changes(&workspace)).unwrap_err();
    let ForkspaceError::PatchIncomplete { omitted, .. } = error else {
        panic!("expected PatchIncomplete");
    };
    assert_eq!(omitted.len(), 1, "{omitted:?}");
    assert!(omitted[0].starts_with("blob (binary"), "{omitted:?}");
}

/// A touch that leaves bytes and mode alone is no change to Git, and not an omission.
#[test]
#[serial]
fn a_rewrite_with_the_same_bytes_exports_nothing() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("same"), "same\n").unwrap();
    let workspace = fork(base.path(), view.path(), "touched");
    build_manifest(&workspace);
    fs::write(view.path().join("same"), "same\n").unwrap();

    let changes = changes(&workspace);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert!(merge::generate_patch(&workspace, view.path(), &changes)
        .unwrap()
        .is_empty());
}

/// A deleted directory is exported with its fork-time contents, so a base edit inside it makes
/// `git apply` refuse instead of being deleted (the verifier's defect 1).
#[test]
#[serial]
fn a_deleted_directory_carries_its_fork_time_contents_not_the_current_base() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::create_dir_all(base.path().join("dd")).unwrap();
    fs::write(base.path().join("dd/a.txt"), "orig\n").unwrap();
    let workspace = fork(base.path(), view.path(), "deleted-dir");
    build_manifest(&workspace);
    fs::remove_dir_all(view.path().join("dd")).unwrap();
    fs::write(base.path().join("dd/a.txt"), "base edit\n").unwrap();
    fs::write(base.path().join("dd/new.txt"), "base added\n").unwrap();

    let patch = merge::generate_patch(&workspace, view.path(), &changes(&workspace)).unwrap();
    let text = String::from_utf8(patch).unwrap();
    assert!(text.contains("-orig\n"), "{text}");
    assert!(!text.contains("base edit"), "{text}");
    assert!(!text.contains("new.txt"), "{text}");
}

/// Everything the directory held at fork goes, ignored contents included, as a direct merge
/// removes them (defect 4).
#[test]
#[serial]
fn a_deleted_directory_takes_its_ignored_contents_with_it() {
    if !git_available() {
        return;
    }
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::create_dir_all(base.path().join("dd/node_modules/p")).unwrap();
    fs::write(base.path().join("dd/a.txt"), "a\n").unwrap();
    fs::write(base.path().join("dd/node_modules/p/i.js"), "m\n").unwrap();
    fs::write(base.path().join("dd/.DS_Store"), "x\n").unwrap();
    let workspace = fork(base.path(), view.path(), "hidden-inside");
    build_manifest(&workspace);
    fs::remove_dir_all(view.path().join("dd")).unwrap();

    let patch = merge::generate_patch(&workspace, view.path(), &changes(&workspace)).unwrap();
    let applied = apply(base.path(), &patch);
    assert!(!applied.path().join("dd").exists());
}

/// An empty directory inside a deleted one cannot be deleted by Git, so the export refuses.
#[test]
#[serial]
fn an_empty_directory_inside_a_deleted_one_refuses_the_patch() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::create_dir_all(base.path().join("dd/emptysub")).unwrap();
    fs::write(base.path().join("dd/a.txt"), "a\n").unwrap();
    let workspace = fork(base.path(), view.path(), "empty-inside");
    build_manifest(&workspace);
    fs::remove_dir_all(view.path().join("dd")).unwrap();

    let error = merge::generate_patch(&workspace, view.path(), &changes(&workspace)).unwrap_err();
    let ForkspaceError::PatchIncomplete { omitted, .. } = error else {
        panic!("expected PatchIncomplete");
    };
    assert!(omitted[0].starts_with("dd/emptysub ("), "{omitted:?}");
}

/// A directory replaced by a file is reported twice by the scan (the file, and the directory's
/// entry as a deletion); the patch must delete it once (defect 2), before or after the walk.
#[test]
#[serial]
fn directory_and_file_swaps_apply_cleanly_before_and_after_the_walk() {
    if !git_available() {
        return;
    }
    for early in [false, true] {
        let _home = common::setup_test_env();
        let base = tempfile::tempdir().unwrap();
        let view = tempfile::tempdir().unwrap();
        fs::create_dir_all(base.path().join("d2file/sub")).unwrap();
        fs::write(base.path().join("d2file/inner.txt"), "in\n").unwrap();
        fs::write(base.path().join("d2file/sub/deep.txt"), "deep\n").unwrap();
        fs::write(base.path().join("f2dir"), "file\n").unwrap();
        fs::write(base.path().join("k.txt"), "k\n").unwrap();
        let workspace = fork(base.path(), view.path(), "swaps");
        let swap = || {
            let v = view.path();
            fs::remove_dir_all(v.join("d2file")).unwrap();
            fs::write(v.join("d2file"), "now\n").unwrap();
            fs::remove_file(v.join("f2dir")).unwrap();
            fs::create_dir(v.join("f2dir")).unwrap();
            fs::write(v.join("f2dir/inner.txt"), "inner\n").unwrap();
            fs::write(v.join("k.txt"), "k2\n").unwrap();
        };
        if early {
            swap();
            build_manifest(&workspace);
        } else {
            build_manifest(&workspace);
            swap();
        }
        let patch = merge::generate_patch(&workspace, view.path(), &changes(&workspace)).unwrap();
        let applied = apply(base.path(), &patch);
        assert_same_tree(applied.path(), view.path());
        forkspace::sync::ancestor::remove("swaps").unwrap();
        forkspace::changes::manifest::remove("swaps").unwrap();
    }
}
