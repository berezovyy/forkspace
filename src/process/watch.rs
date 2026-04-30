//! Process liveness with PID-reuse protection.
//!
//! Each spawned process gets an identity token (env var, or start time as a fallback). A
//! recorded PID is treated as alive only if its current identity still matches, so stale
//! state never signals an unrelated process that recycled the PID.

#[cfg(target_os = "macos")]
use std::process::Command;

use crate::util::now_unix_ns;

/// Environment variable carrying the identity token of a process forkspace spawned.
pub(crate) const IDENTITY_ENV: &str = "FORKSPACE_PROCESS_IDENTITY";

/// The identity of the running process `pid`: its [`IDENTITY_ENV`] value if set, otherwise
/// its start time. `None` if the process cannot be inspected.
pub fn identity(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ")) {
            if let Some(value) = bytes
                .split(|byte| *byte == 0)
                .find_map(|entry| env_value(entry, IDENTITY_ENV.as_bytes()))
            {
                return Some(String::from_utf8_lossy(value).into_owned());
            }
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)
            .map(str::to_string)
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = Command::new("ps")
            .args(["eww", "-p", &pid.to_string(), "-o", "command="])
            .output()
        {
            if let Some(value) = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .find_map(|field| env_value(field.as_bytes(), IDENTITY_ENV.as_bytes()))
            {
                return Some(String::from_utf8_lossy(value).into_owned());
            }
        }
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

/// The value of `entry` if it is the assignment `key=value`.
fn env_value<'a>(entry: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    entry.strip_prefix(key)?.strip_prefix(b"=")
}

/// A fresh identity token to hand a child process through [`IDENTITY_ENV`].
pub fn new_identity() -> String {
    format!("forkspace-{}-{}", std::process::id(), now_unix_ns())
}

#[cfg(target_os = "macos")]
fn kqueue_alive(pid: u32) -> bool {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return false;
    }
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let registered =
        unsafe { libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } == 0;
    let _ = unsafe { libc::close(queue) };
    registered
}

/// PIDs that `kill(2)` would interpret as "my group" or "every process".
fn is_signalable(pid: u32) -> bool {
    pid != 0 && pid <= i32::MAX as u32
}

/// Whether `pid` is running and, when `expected_identity` is given, is still that process.
pub fn is_alive(pid: u32, expected_identity: Option<&str>) -> bool {
    if !is_signalable(pid) {
        return false;
    }
    if expected_identity.is_some_and(|expected| identity(pid).as_deref() != Some(expected)) {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        kqueue_alive(pid)
    }
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

/// SIGTERM the process group led by `pid`, escalating to SIGKILL after 200ms.
/// Returns whether a matching live process was found.
pub fn terminate_group(pid: u32, expected_identity: Option<&str>) -> bool {
    if pid == 1 || !is_alive(pid, expected_identity) {
        return false;
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    if is_alive(pid, expected_identity) {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    true
}
