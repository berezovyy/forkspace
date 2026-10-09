#![allow(dead_code)]

use std::path::Path;

use forkspace::util::now_rfc3339;
use forkspace::workspace::Workspace;

pub fn setup_test_env() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    // SAFETY: tests using setup_test_env() are serialized with #[serial],
    // so no concurrent env var mutation can occur.
    unsafe { std::env::set_var("FORKSPACE_HOME", dir.path()) };
    // Commands would otherwise leave a systemd slice per test workspace that is never
    // dropped; `tests/processes.rs` covers the systemd path.
    unsafe { std::env::set_var("FORKSPACE_NO_SYSTEMD", "1") };
    dir
}

pub fn create_test_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).expect("create src dir");
    std::fs::write(dir.join("README.md"), "# Test Project\n").expect("write README");
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"test-project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
}

pub fn setup_with_project() -> (tempfile::TempDir, tempfile::TempDir) {
    let forkspace_dir = setup_test_env();
    let base = tempfile::tempdir().expect("failed to create temp dir");
    create_test_project(base.path());
    (forkspace_dir, base)
}

pub fn fork_helper(
    base: std::path::PathBuf,
    name: &str,
) -> forkspace::error::Result<forkspace::workspace::Workspace> {
    forkspace::workspace::create(base, name, Default::default())
}

pub fn make_test_workspace(
    root: &Path,
    name: &str,
    base: &Path,
    parent: Option<String>,
) -> Workspace {
    let ws_dir = root.join(name);
    std::fs::create_dir_all(ws_dir.join("upper")).expect("create upper");
    std::fs::create_dir_all(ws_dir.join("work")).expect("create work");
    std::fs::create_dir_all(ws_dir.join("merged")).expect("create merged");

    let mut workspace = Workspace::new(name, base.to_path_buf(), parent);
    workspace.created = now_rfc3339();
    workspace.view_path = ws_dir.join("merged");
    if let forkspace::backend::BackendState::Overlay(state) = &mut workspace.backend_state {
        state.upper = ws_dir.join("upper");
        state.work = ws_dir.join("work");
        state.merged = ws_dir.join("merged");
    }
    workspace
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn cleanup_workspace(ws: &forkspace::workspace::Workspace) {
    let _ = forkspace::backend::overlay::mount::unmount_overlay(&ws.view_path);
}

#[macro_export]
macro_rules! skip_without_overlay {
    () => {{
        if let Err(reason) = forkspace::backend::overlay::mount::probe_fuse_overlayfs() {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"fuse-overlayfs\",\"reason\":{}}}",
                serde_json::to_string(&reason).expect("serialize skip reason")
            );
            return;
        }
    }};
}
