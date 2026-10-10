//! The write sandbox: commands started by `enter` and `run` may write inside their workspace,
//! but the OS refuses their writes to the original project and to other workspaces.
//!
//! Linux uses Landlock (kernel 5.13+, no privileges): the parent builds a ruleset that handles
//! every write-type right and allows them only beneath the workspace view and the usual
//! scratch and cache locations; the child calls `landlock_restrict_self` just before `exec`,
//! so the command and everything it starts inherit the restriction. Reading is not restricted.
//! Landlock can only allow, so an allowed location that contains the project or
//! `FORKSPACE_HOME` (for example a project under `/tmp`) is left out entirely.
//!
//! macOS runs the command under `sandbox-exec` with a profile that denies writes beneath the
//! project and `FORKSPACE_HOME` and allows them beneath the workspace view.
//!
//! `--no-sandbox` or `FORKSPACE_SANDBOX=off` turns it off. `no_new_privs`, which Landlock
//! requires, means setuid programs such as `sudo` gain no privileges inside the sandbox.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths;
use crate::workspace::Workspace;

/// Whether the sandbox is turned off for every command (`FORKSPACE_SANDBOX=off`).
pub fn disabled_by_default() -> bool {
    std::env::var("FORKSPACE_SANDBOX")
        .is_ok_and(|value| matches!(value.as_str(), "off" | "0" | "false"))
}

/// The directories commands in `workspace` may write beneath.
pub fn writable(workspace: &Workspace) -> Vec<PathBuf> {
    let protected: Vec<PathBuf> = [workspace.base.clone(), paths::forkspace_home()]
        .into_iter()
        .map(|path| canonical(&path))
        .collect();
    let mut candidates = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev/shm"),
        PathBuf::from("/dev"),
        PathBuf::from("/proc"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/folders"),
    ];
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime));
    }
    if let Some(temporary) = std::env::var_os("TMPDIR") {
        candidates.push(PathBuf::from(temporary));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let entries = std::fs::read_dir(home).into_iter().flatten().flatten();
        candidates.extend(
            entries
                .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
                .map(|entry| entry.path()),
        );
    }
    let mut allowed: Vec<PathBuf> = candidates
        .iter()
        .filter(|path| path.exists())
        .map(|path| canonical(path))
        .filter(|candidate| {
            protected.iter().all(|protected| {
                !protected.starts_with(candidate) && !candidate.starts_with(protected)
            })
        })
        .collect();
    allowed.push(canonical(&workspace.view_path));
    allowed.sort();
    allowed.dedup();
    allowed
}

/// Restrict `command`'s writes to `workspace`, or explain once why that is not possible.
pub fn apply(command: Command, workspace: &Workspace) -> Command {
    imp::apply(command, workspace)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use super::writable;
    use crate::workspace::Workspace;

    const SYS_CREATE_RULESET: libc::c_long = 444;
    const SYS_ADD_RULE: libc::c_long = 445;
    const SYS_RESTRICT_SELF: libc::c_long = 446;
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    const WRITE_FILE: u64 = 1 << 1;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    /// ABI 2: linking or renaming across directories.
    const REFER: u64 = 1 << 13;
    /// ABI 3.
    const TRUNCATE: u64 = 1 << 14;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    pub fn apply(mut command: Command, workspace: &Workspace) -> Command {
        let ruleset = match ruleset(workspace) {
            Ok(ruleset) => ruleset,
            Err(reason) => {
                eprintln!("warning: the workspace write sandbox is off: {reason}");
                return command;
            }
        };
        // SAFETY: only async-signal-safe syscalls run between fork and exec. The closure owns
        // the ruleset, so its descriptor stays open for as long as the command can spawn; in
        // the child it is closed by exec (it is close-on-exec).
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::syscall(SYS_RESTRICT_SELF, ruleset.as_raw_fd(), 0) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }

    /// A ruleset allowing writes only beneath [`writable`] paths.
    fn ruleset(workspace: &Workspace) -> Result<OwnedFd, String> {
        if std::env::var_os("FORKSPACE_NO_LANDLOCK").is_some() {
            return Err("Landlock is disabled (FORKSPACE_NO_LANDLOCK)".to_string());
        }
        let abi = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                std::ptr::null::<RulesetAttr>(),
                0_usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            return Err("this kernel does not provide Landlock".to_string());
        }
        let mut handled = WRITE_FILE
            | REMOVE_DIR
            | REMOVE_FILE
            | MAKE_CHAR
            | MAKE_DIR
            | MAKE_REG
            | MAKE_SOCK
            | MAKE_FIFO
            | MAKE_BLOCK
            | MAKE_SYM;
        if abi >= 2 {
            handled |= REFER;
        }
        if abi >= 3 {
            handled |= TRUNCATE;
        }
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        let raw = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0_u32,
            )
        };
        if raw < 0 {
            return Err(format!(
                "creating a Landlock ruleset failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: the syscall returned a new file descriptor that we now own.
        let ruleset = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        for path in writable(workspace) {
            let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
                continue;
            };
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                continue;
            }
            // SAFETY: `open` returned a new descriptor that we now own.
            let parent = unsafe { OwnedFd::from_raw_fd(fd) };
            let is_dir = std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
            let allowed = if is_dir {
                handled
            } else {
                handled & (WRITE_FILE | TRUNCATE)
            };
            let rule = PathBeneathAttr {
                allowed_access: allowed,
                parent_fd: parent.as_raw_fd(),
            };
            let added = unsafe {
                libc::syscall(
                    SYS_ADD_RULE,
                    ruleset.as_raw_fd(),
                    RULE_PATH_BENEATH,
                    &rule as *const PathBeneathAttr,
                    0_u32,
                )
            };
            if added != 0 {
                return Err(format!(
                    "allowing writes beneath {} failed: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(ruleset)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::Path;
    use std::process::Command;

    use super::canonical;
    use crate::paths;
    use crate::workspace::Workspace;

    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    pub fn apply(command: Command, workspace: &Workspace) -> Command {
        if !Path::new(SANDBOX_EXEC).exists() {
            eprintln!("warning: the workspace write sandbox is off: sandbox-exec is missing");
            return command;
        }
        let mut wrapped = Command::new(SANDBOX_EXEC);
        wrapped
            .arg("-p")
            .arg(profile(workspace))
            .arg("--")
            .arg(command.get_program())
            .args(command.get_args());
        if let Some(dir) = command.get_current_dir() {
            wrapped.current_dir(dir);
        }
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => wrapped.env(key, value),
                None => wrapped.env_remove(key),
            };
        }
        wrapped
    }

    /// Allow everything, deny writes beneath the project and `FORKSPACE_HOME`, then allow
    /// them again beneath the view (the later, more specific rule wins).
    fn profile(workspace: &Workspace) -> String {
        let quote = |path: &Path| {
            format!(
                "\"{}\"",
                canonical(path)
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            )
        };
        format!(
            "(version 1)\n(allow default)\n(deny file-write* (subpath {}) (subpath {}))\n(allow file-write* (subpath {}))\n",
            quote(&workspace.base),
            quote(&paths::forkspace_home()),
            quote(&workspace.view_path)
        )
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::process::Command;

    use crate::workspace::Workspace;

    pub fn apply(command: Command, _workspace: &Workspace) -> Command {
        command
    }
}
