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
    // fusermount3 refuses some mounts (EPERM) that the older fusermount still unmounts.
    let tools: Vec<&str> = ["/usr/bin/fusermount3", "/usr/bin/fusermount"]
        .into_iter()
        .filter(|path| Path::new(path).exists())
        .collect();
    for point in points {
        for tool in &tools {
            let unmounted = std::process::Command::new(tool)
                .args(["-u", "-z"])
                .arg(&point)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if unmounted {
                break;
            }
        }
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

/// `forkspace new <args>` run from `project`, the folder it copies.
pub fn new_in(home: &Path, project: impl AsRef<Path>, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home)
        .current_dir(project)
        .arg("new")
        .args(args)
        .output()
        .unwrap()
}

/// Where workspace `name` of the home `home` is: an overlay's merged view, or a copy's folder.
pub fn view_of(home: &Path, name: &str) -> PathBuf {
    let overlay = home.join(name).join("merged");
    if overlay.is_dir() {
        overlay
    } else {
        home.join("workspaces").join(name)
    }
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
    forkspace::changes::manifest::set_builder_executable(env!("CARGO_BIN_EXE_forkspace").into());
}

/// `command` under strace, with every rename delayed by `delay` before it runs. Each step of a
/// merge, pull or restore installs or moves its entry with a rename, and so does each save of
/// its journal, so a test gets time to act between steps without a hook in forkspace.
/// `--kill-on-exit`: killing the returned child kills forkspace too. `None`, after a skip
/// line, where strace cannot trace (macOS, strace missing, ptrace refused).
pub fn with_slow_renames(
    command: &std::process::Command,
    delay: std::time::Duration,
) -> Option<std::process::Command> {
    static TRACEABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let traceable = *TRACEABLE.get_or_init(|| {
        cfg!(target_os = "linux")
            && std::process::Command::new("strace")
                .args(["-f", "--kill-on-exit", "-o", "/dev/null", "true"])
                .output()
                .is_ok_and(|output| output.status.success())
    });
    if !traceable {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"strace\",\"reason\":\"strace cannot trace here\"}}"
        );
        return None;
    }
    let renames = "rename,renameat,renameat2";
    let mut traced = std::process::Command::new("strace");
    traced
        .args(["-f", "--kill-on-exit", "-o", "/dev/null", "-e"])
        .arg(format!("trace={renames}"))
        .arg("-e")
        .arg(format!(
            "inject={renames}:delay_enter={}",
            delay.as_micros()
        ))
        .arg(command.get_program())
        .args(command.get_args());
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => traced.env(key, value),
            None => traced.env_remove(key),
        };
    }
    if let Some(directory) = command.get_current_dir() {
        traced.current_dir(directory);
    }
    Some(traced)
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

/// Return early (with a skip line) where `new` would not copy: fuse-overlayfs works here, so
/// workspaces are overlays. For tests of what only a copied workspace does.
#[macro_export]
macro_rules! skip_unless_copying {
    () => {{
        if forkspace::backend::overlay::mount::probe_fuse_overlayfs_fresh().is_ok() {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"copy\",\"reason\":\"fuse-overlayfs works here, so workspaces are overlays\"}}"
            );
            return;
        }
    }};
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
