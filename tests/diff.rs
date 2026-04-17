mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use forkspace::diff::{collect_changes, is_binary, ChangeKind, DiffFormat, DiffOutput};
use forkspace::filter::{ChangeClass, ChangeFilter};
use serial_test::serial;

#[test]
fn test_diff_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("file.txt"), "hello\n").unwrap();

    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);
    let changes = collect_changes(&ws).unwrap();
    assert!(
        changes.is_empty(),
        "expected no changes, got: {:?}",
        changes
    );
}

#[test]
fn test_diff_added_file() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::write(
        ws.upper_path().unwrap().join("new_file.rs"),
        "fn main() {}\n",
    )
    .unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0].kind, ChangeKind::Added));
    assert_eq!(changes[0].path, PathBuf::from("new_file.rs"));
    assert_eq!(changes[0].additions, Some(1));
    assert!(!changes[0].is_binary);
}

#[test]
fn test_diff_modified_file() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::write(base.join("file.txt"), "line1\nline2\nline3\n").unwrap();
    fs::write(
        ws.upper_path().unwrap().join("file.txt"),
        "line1\nmodified\nline3\nnew_line\n",
    )
    .unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0].kind, ChangeKind::Modified));
    assert_eq!(changes[0].additions, Some(2));
    assert_eq!(changes[0].deletions, Some(1));
}

#[test]
fn test_diff_deleted_file() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::write(base.join("removed.txt"), "content\n").unwrap();
    fs::write(ws.upper_path().unwrap().join(".wh.removed.txt"), "").unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0].kind, ChangeKind::Deleted));
    assert_eq!(changes[0].path, PathBuf::from("removed.txt"));
}

#[test]
fn test_diff_files_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::write(ws.upper_path().unwrap().join("a.txt"), "hello\n").unwrap();
    fs::write(ws.upper_path().unwrap().join("b.txt"), "world\n").unwrap();

    let changes = collect_changes(&ws).unwrap();
    let output = forkspace::diff::format_diff(&changes, &DiffFormat::Files);
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines.contains(&"a.txt"));
    assert!(lines.contains(&"b.txt"));
}

#[test]
fn test_diff_stat_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::write(ws.upper_path().unwrap().join("new.txt"), "new\n").unwrap();
    fs::write(base.join("changed.txt"), "old\n").unwrap();
    fs::write(
        ws.upper_path().unwrap().join("changed.txt"),
        "new content\n",
    )
    .unwrap();
    fs::write(base.join("gone.txt"), "bye\n").unwrap();
    fs::write(ws.upper_path().unwrap().join(".wh.gone.txt"), "").unwrap();

    let changes = collect_changes(&ws).unwrap();
    let output = forkspace::diff::format_diff(&changes, &DiffFormat::Stat);
    assert!(output.contains("3 files changed"));
    assert!(output.contains("1 added"));
    assert!(output.contains("1 modified"));
    assert!(output.contains("1 deleted"));
}

#[test]
fn test_diff_json_output() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::write(ws.upper_path().unwrap().join("added.rs"), "fn hello() {}\n").unwrap();

    let changes = collect_changes(&ws).unwrap();
    let output = DiffOutput {
        workspace: "test-ws".to_string(),
        base: base.clone(),
        changes,
    };
    let json_str = serde_json::to_string_pretty(&output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed["workspace"], "test-ws");
    let first = &parsed["changes"][0];
    assert_eq!(first["kind"], "added");
    assert_eq!(first["class"], "source");
    assert_eq!(first["path"], "added.rs");
    assert!(!first["is_binary"].as_bool().unwrap());
}

#[test]
fn test_diff_reports_chmod_only_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("script"), "same\n").unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);
    fs::write(ws.upper_path().unwrap().join("script"), "same\n").unwrap();
    let mut permissions = fs::metadata(ws.upper_path().unwrap().join("script"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(ws.upper_path().unwrap().join("script"), permissions).unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0].kind, ChangeKind::Modified));
    assert_eq!(
        (changes[0].additions, changes[0].deletions),
        (Some(0), Some(0))
    );
}

#[test]
fn test_change_classes_and_lockfile_visibility_are_shared() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(base.join(".git")).unwrap();
    fs::write(base.join(".gitignore"), "Cargo.lock\n").unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);
    fs::create_dir_all(ws.upper_path().unwrap().join(".git")).unwrap();
    fs::write(ws.upper_path().unwrap().join(".git/index"), "index").unwrap();
    fs::write(ws.upper_path().unwrap().join(".DS_Store"), "finder").unwrap();
    fs::write(ws.upper_path().unwrap().join("Cargo.lock"), "lock").unwrap();
    fs::write(ws.upper_path().unwrap().join("source.rs"), "source").unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(
        changes
            .iter()
            .find(|change| change.path == std::path::Path::new(".git/index"))
            .unwrap()
            .class,
        ChangeClass::Internal
    );
    assert_eq!(
        changes
            .iter()
            .find(|change| change.path == std::path::Path::new(".DS_Store"))
            .unwrap()
            .class,
        ChangeClass::Runtime
    );
    let visible = ChangeFilter::build(&base).filter_changes(changes);
    assert_eq!(visible.len(), 2);
    assert!(visible
        .iter()
        .any(|change| change.path == std::path::Path::new("Cargo.lock")
            && change.class == ChangeClass::Dependency));
    assert!(visible
        .iter()
        .any(|change| change.path == std::path::Path::new("source.rs")));
}

#[test]
fn test_diff_binary_detection() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    let mut binary_content = vec![0x89, 0x50, 0x4E, 0x47];
    binary_content.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D]);
    binary_content.extend_from_slice(&[0xFF; 100]);
    fs::write(ws.upper_path().unwrap().join("image.png"), &binary_content).unwrap();
    fs::write(ws.upper_path().unwrap().join("text.txt"), "hello world\n").unwrap();

    assert!(is_binary(&ws.upper_path().unwrap().join("image.png")));
    assert!(!is_binary(&ws.upper_path().unwrap().join("text.txt")));

    let changes = collect_changes(&ws).unwrap();
    let binary_change = changes
        .iter()
        .find(|c| c.path == std::path::Path::new("image.png"))
        .unwrap();
    assert!(binary_change.is_binary);
    assert!(binary_change.additions.is_none());

    let text_change = changes
        .iter()
        .find(|c| c.path == std::path::Path::new("text.txt"))
        .unwrap();
    assert!(!text_change.is_binary);
}

#[test]
fn test_diff_nested_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::create_dir_all(ws.upper_path().unwrap().join("src/nested")).unwrap();
    fs::write(
        ws.upper_path().unwrap().join("src/nested/deep.rs"),
        "fn deep() {}\n",
    )
    .unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, PathBuf::from("src/nested/deep.rs"));
}

#[test]
fn test_diff_opaque_whiteout_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(base.join("subdir")).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::create_dir_all(ws.upper_path().unwrap().join("subdir")).unwrap();
    fs::write(ws.upper_path().unwrap().join("subdir/.wh..wh..opq"), "").unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert!(changes.is_empty());
}

#[test]
#[serial]
fn test_diff_command_workspace_not_found() {
    let _dir = common::setup_test_env();
    let result = forkspace::commands::diff::exec(forkspace::cli::DiffArgs {
        name: "nonexistent".to_string(),
        stat: false,
        full: false,
        files: false,
        cumulative: false,
        json: false,
        no_gitignore: false,
    });
    assert!(matches!(
        result.unwrap_err(),
        forkspace::error::ForkspaceError::WorkspaceNotFound(_)
    ));
}

#[test]
fn test_diff_whiteout_in_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(base.join("src")).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::create_dir_all(ws.upper_path().unwrap().join("src")).unwrap();
    fs::write(base.join("src/old.rs"), "fn old() {}\n").unwrap();
    fs::write(ws.upper_path().unwrap().join("src/.wh.old.rs"), "").unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0].kind, ChangeKind::Deleted));
    assert_eq!(changes[0].path, PathBuf::from("src/old.rs"));
}

#[test]
fn test_empty_dot_wh_named_file_is_not_a_false_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);
    fs::write(ws.upper_path().unwrap().join(".wh.literal"), b"").unwrap();

    let changes = collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, PathBuf::from(".wh.literal"));
    assert!(matches!(changes[0].kind, ChangeKind::Added));
}

#[test]
fn test_diff_default_format() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(&base).unwrap();
    let ws = common::make_test_workspace(tmp.path(), "test-ws", &base, None);

    fs::write(base.join("mod.rs"), "line1\n").unwrap();
    fs::write(ws.upper_path().unwrap().join("mod.rs"), "line1\nline2\n").unwrap();
    fs::write(ws.upper_path().unwrap().join("new.rs"), "fn new() {}\n").unwrap();

    let changes = collect_changes(&ws).unwrap();
    let output = forkspace::diff::format_diff(&changes, &DiffFormat::Default);
    let plain = common::strip_ansi(&output);
    assert!(
        plain.contains("M  mod.rs"),
        "expected 'M  mod.rs' in: {plain}"
    );
    assert!(
        plain.contains("A  new.rs"),
        "expected 'A  new.rs' in: {plain}"
    );
}
