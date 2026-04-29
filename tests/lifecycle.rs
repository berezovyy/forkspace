mod common;

use std::fs;

use forkspace::changes;
use forkspace::sync::merge;

#[test]
fn test_full_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    fs::create_dir_all(base.join("src")).unwrap();
    fs::write(base.join("README.md"), "# My Project\n").unwrap();
    fs::write(base.join("src/main.rs"), "fn main() {}\n").unwrap();

    let ws = common::make_test_workspace(tmp.path(), "lifecycle-ws", &base, None);

    fs::create_dir_all(ws.upper_path().unwrap().join("src")).unwrap();
    fs::write(
        ws.upper_path().unwrap().join("src/lib.rs"),
        "pub fn hello() {}\n",
    )
    .unwrap();
    fs::write(
        ws.upper_path().unwrap().join("src/main.rs"),
        "use crate::lib::hello;\nfn main() { hello(); }\n",
    )
    .unwrap();
    fs::write(ws.upper_path().unwrap().join(".wh.README.md"), "").unwrap();

    let changes = changes::collect_changes(&ws).unwrap();
    assert_eq!(changes.len(), 3, "expected 3 changes, got: {changes:?}");

    let added = changes
        .iter()
        .filter(|c| matches!(c.kind, changes::ChangeKind::Added))
        .count();
    let modified = changes
        .iter()
        .filter(|c| matches!(c.kind, changes::ChangeKind::Modified))
        .count();
    let deleted = changes
        .iter()
        .filter(|c| matches!(c.kind, changes::ChangeKind::Deleted))
        .count();
    assert_eq!(added, 1);
    assert_eq!(modified, 1);
    assert_eq!(deleted, 1);

    let patch = merge::generate_patch(&ws, &changes).unwrap();
    assert!(!patch.is_empty());
    assert!(patch.contains("src/lib.rs"));
    assert!(patch.contains("src/main.rs"));
    assert!(patch.contains("README.md"));

    let result = merge::merge_workspace(&ws).unwrap();
    assert_eq!(result.added, 1);
    assert_eq!(result.modified, 1);
    assert_eq!(result.deleted, 1);

    assert!(base.join("src/lib.rs").exists());
    assert_eq!(
        fs::read_to_string(base.join("src/lib.rs")).unwrap(),
        "pub fn hello() {}\n"
    );
    assert_eq!(
        fs::read_to_string(base.join("src/main.rs")).unwrap(),
        "use crate::lib::hello;\nfn main() { hello(); }\n"
    );
    assert!(!base.join("README.md").exists());
}
