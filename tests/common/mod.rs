#![allow(dead_code)]

pub mod cli;

use std::path::{Path, PathBuf};

use forkspace::util::now_rfc3339;
use forkspace::workspace::Workspace;

/// A scratch `FORKSPACE_HOME`. Dropping it unmounts every overlay mounted under it, so a test
/// that leaves workspaces behind (or panics before dropping them) does not leave a
/// fuse-overlayfs daemon running and a mount in the temporary directory.
pub struct TestHome(tempfile::TempDir);

impl TestHome {
    pub fn new() -> Self {
        Self(tempfile::tempdir().expect("failed to create temp dir"))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Default for TestHome {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        unmount_under(self.0.path());
        // The private TMPDIRs of this home's workspaces live outside it.
        let _ = std::fs::remove_dir_all(forkspace::paths::private_tmp_home_of(self.0.path()));
    }
}

/// Unmount every mount below `root`, deepest first (Linux; elsewhere forkspace mounts none).
pub fn unmount_under(root: &Path) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut points: Vec<PathBuf> = info
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(unescape_mountinfo(field)))
        .filter(|point| point.starts_with(&root) && point != &root)
        .collect();
    points.sort_by_key(|point| std::cmp::Reverse(point.components().count()));
    let fusermount = ["/usr/bin/fusermount3", "/usr/bin/fusermount"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .unwrap_or("fusermount3");
    for point in points {
        let _ = std::process::Command::new(fusermount)
            .args(["-u", "-z"])
            .arg(&point)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// `/proc/self/mountinfo` writes space, tab, newline and backslash in paths as `\ooo`.
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&field[index + 1..index + 4], 8) {
                out.push(value);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn setup_test_env() -> TestHome {
    let dir = TestHome::new();
    // SAFETY: tests using setup_test_env() are serialized with #[serial],
    // so no concurrent env var mutation can occur.
    unsafe { std::env::set_var("FORKSPACE_HOME", dir.path()) };
    // Run as without a systemd user manager (an SSH session without lingering): commands
    // would otherwise leave a slice per test workspace that is never dropped.
    // `tests/processes.rs` covers the systemd path.
    unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    use_forkspace_for_workers();
    dir
}

/// Background manifest builders re-run the current binary, which in a test is the test
/// binary itself; point them at the forkspace binary instead.
pub fn use_forkspace_for_workers() {
    // SAFETY: as above, tests that set the environment are serialized.
    unsafe {
        std::env::set_var(
            "FORKSPACE_INTERNAL_MANIFEST_EXE",
            env!("CARGO_BIN_EXE_forkspace"),
        )
    };
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

pub fn setup_with_project() -> (TestHome, tempfile::TempDir) {
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
        if let Err(reason) = forkspace::backend::overlay::mount::probe_fuse_overlayfs_fresh() {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"fuse-overlayfs\",\"reason\":{}}}",
                serde_json::to_string(&reason).expect("serialize skip reason")
            );
            return;
        }
    }};
}
