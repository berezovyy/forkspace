//! Mounting and unmounting fuse-overlayfs views, with actionable diagnostics when the host
//! cannot mount FUSE filesystems.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use crate::error::{ForkspaceError, Result};
use crate::util::now_unix_ns;

pub(crate) const DEFAULT_TMPFS_SIZE: &str = "256m";

pub(crate) struct OverlayOpts {
    pub tmpfs: bool,
    pub tmpfs_size: String,
}

impl Default for OverlayOpts {
    fn default() -> Self {
        Self {
            tmpfs: false,
            tmpfs_size: DEFAULT_TMPFS_SIZE.to_string(),
        }
    }
}

/// Mounts and immediately unmounts a scratch overlay to prove fuse-overlayfs works here.
/// The error is a human-readable reason.
pub fn probe_fuse_overlayfs() -> std::result::Result<(), String> {
    open_fuse_device().map_err(|e| format!("cannot open /dev/fuse: {e}"))?;

    let root = std::env::temp_dir().join(format!(
        "forkspace-fuse-probe-{}-{}",
        std::process::id(),
        now_unix_ns()
    ));
    let lower = root.join("lower");
    let upper = root.join("upper");
    let work = root.join("work");
    let merged = root.join("merged");
    for path in [&lower, &upper, &work, &merged] {
        fs::create_dir_all(path)
            .map_err(|e| format!("cannot create probe directory {}: {e}", path.display()))?;
    }

    let result = mount_overlay(&lower, &upper, &work, &merged, &OverlayOpts::default())
        .map_err(|e| e.to_string())
        .and_then(|()| unmount_overlay(&merged).map_err(|e| e.to_string()));
    let _ = fs::remove_dir_all(&root);
    result
}

/// The error for `--tmpfs`, which overlay workspaces do not support yet.
pub(crate) fn tmpfs_not_implemented() -> ForkspaceError {
    ForkspaceError::CapabilityUnsupported {
        diagnosis: "--tmpfs is not implemented".to_string(),
        remedy: "omit --tmpfs to use disk-backed workspace storage".to_string(),
    }
}

fn mount_tmpfs(_path: &Path, _size: &str) -> Result {
    Err(tmpfs_not_implemented())
}

/// Best-effort teardown of a tmpfs-backed layer directory; failures are only warned about.
pub(crate) fn unmount_tmpfs(path: &Path) -> Result {
    if let Err(e) = fuse_unmount(path) {
        eprintln!("warning: failed to unmount {}: {e}", path.display());
    }
    if path.exists() {
        if let Err(e) = fs::remove_dir_all(path) {
            eprintln!(
                "warning: failed to remove directory {}: {e}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Mounts `base` (lower) + `upper` at `merged`.
pub(crate) fn mount_overlay(
    base: &Path,
    upper: &Path,
    work: &Path,
    merged: &Path,
    opts: &OverlayOpts,
) -> Result {
    if opts.tmpfs {
        mount_tmpfs(upper, &opts.tmpfs_size)?;
        if let Err(e) = mount_tmpfs(work, &opts.tmpfs_size) {
            let _ = unmount_tmpfs(upper);
            return Err(e);
        }
    }

    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        base.display(),
        upper.display(),
        work.display()
    );
    let status = Command::new(find_fuse_overlayfs())
        .arg("-o")
        .arg(&options)
        .arg(merged.as_os_str())
        .status();
    let error = match status {
        Ok(status) if status.success() => return Ok(()),
        Ok(_) => ForkspaceError::MountFailed(diagnose_mount_failure(merged)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ForkspaceError::BackendUnavailable {
            diagnosis: "fuse-overlayfs was not found".to_string(),
            remedy: "install the fuse-overlayfs package for your Linux distribution".to_string(),
        },
        Err(e) => ForkspaceError::MountFailed(format!("failed to run fuse-overlayfs: {}", e)),
    };

    if opts.tmpfs {
        let _ = unmount_tmpfs(work);
        let _ = unmount_tmpfs(upper);
    }
    Err(error)
}

/// Unmounts the overlay at `merged`. Succeeds when it is not mounted (or cannot be unmounted
/// because fusermount is missing).
pub fn unmount_overlay(merged: &Path) -> Result {
    if is_mounted(merged) == Some(false) {
        return Ok(());
    }
    match fuse_unmount(merged) {
        Err(e) if is_not_mounted_or_missing(&e) => Ok(()),
        result => result,
    }
}

/// Whether `path` is a mount point, per `/proc/self/mountinfo`; `None` if that is unreadable.
fn is_mounted(path: &Path) -> Option<bool> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let path = path.to_string_lossy();
    Some(mountinfo.lines().any(|line| {
        line.split_whitespace()
            .nth(4)
            .is_some_and(|mount_point| unescape_mount_point(mount_point) == path)
    }))
}

/// Undo mountinfo's octal escapes (`\040` for a space, and so on).
fn unescape_mount_point(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes.get(index + 1..index + 4).filter(|digits| {
            bytes[index] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        match escape {
            Some(digits) => {
                let value = digits
                    .iter()
                    .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                decoded.push(value as u8);
                index += 4;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Whether an overlay view is usable right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewHealth {
    Mounted,
    /// Mounted, but the fuse-overlayfs process behind it is gone (`ENOTCONN`).
    Disconnected,
    /// A plain directory: never mounted since boot, or unmounted underneath forkspace.
    Unmounted,
    /// `/proc/self/mountinfo` is unreadable, so mount state cannot be checked.
    Unknown,
}

pub(crate) fn view_health(merged: &Path) -> ViewHealth {
    match is_mounted(merged) {
        None => ViewHealth::Unknown,
        Some(false) => ViewHealth::Unmounted,
        Some(true) => match fs::read_dir(merged) {
            Err(error) if error.raw_os_error() == Some(libc::ENOTCONN) => ViewHealth::Disconnected,
            _ => ViewHealth::Mounted,
        },
    }
}

/// Detach a dead FUSE mount (`fusermount -u -z`) so the mount point can be reused.
pub(crate) fn detach_dead_mount(merged: &Path) -> Result {
    let fusermount = find_fusermount();
    let output = Command::new(fusermount)
        .arg("-u")
        .arg("-z")
        .arg(merged.as_os_str())
        .output()
        .map_err(|e| ForkspaceError::UnmountFailed(format!("failed to run {fusermount}: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ForkspaceError::UnmountFailed(format!(
        "{} -u -z {} failed: {}",
        fusermount,
        merged.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn is_not_mounted_or_missing(error: &ForkspaceError) -> bool {
    let message = error.to_string();
    message.contains("not mounted") || message.contains("not found")
}

fn fuse_unmount(path: &Path) -> Result {
    let fusermount = find_fusermount();
    let output = Command::new(fusermount)
        .arg("-u")
        .arg(path.as_os_str())
        .output();

    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not mounted") || stderr.contains("No such file") {
                return Ok(());
            }
            Err(ForkspaceError::UnmountFailed(format!(
                "{} -u {} failed: {}",
                fusermount,
                path.display(),
                stderr.trim()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ForkspaceError::UnmountFailed(
            "fusermount3 not found — install fuse3".to_string(),
        )),
        Err(e) => Err(ForkspaceError::UnmountFailed(format!(
            "failed to run {}: {}",
            fusermount, e
        ))),
    }
}

fn find_fuse_overlayfs() -> &'static str {
    if Path::new("/usr/bin/fuse-overlayfs").exists() {
        "/usr/bin/fuse-overlayfs"
    } else {
        "fuse-overlayfs"
    }
}

fn find_fusermount() -> &'static str {
    if Path::new("/usr/bin/fusermount3").exists() {
        "/usr/bin/fusermount3"
    } else if Path::new("/usr/bin/fusermount").exists() {
        "/usr/bin/fusermount"
    } else {
        "fusermount3"
    }
}

fn open_fuse_device() -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
}

/// Explains a failed mount with the first host misconfiguration found, or a manual-test hint.
fn diagnose_mount_failure(merged: &Path) -> String {
    let hint = fuse_device_hint()
        .or_else(fusermount_setuid_hint)
        .or_else(user_namespace_hint)
        .unwrap_or_else(|| {
            "  Run with RUST_LOG=debug for more details, or try:\n    \
             fuse-overlayfs -o lowerdir=/tmp/test,upperdir=/tmp/test2,workdir=/tmp/test3 /tmp/test4\n  \
             to test manually."
                .to_string()
        });
    format!(
        "fuse-overlayfs failed to mount overlay at {}\n{hint}",
        merged.display()
    )
}

fn fuse_device_hint() -> Option<String> {
    if !Path::new("/dev/fuse").exists() {
        return Some(
            "  /dev/fuse not found. Load the FUSE kernel module:\n    sudo modprobe fuse"
                .to_string(),
        );
    }
    if open_fuse_device().is_err() {
        return Some(
            "  /dev/fuse is not accessible. Fix permissions:\n    \
             sudo chmod 666 /dev/fuse\n  \
             or add yourself to the 'fuse' group:\n    \
             sudo usermod -aG fuse $USER && newgrp fuse"
                .to_string(),
        );
    }
    None
}

fn fusermount_setuid_hint() -> Option<String> {
    let fusermount = find_fusermount();
    let metadata = fs::metadata(fusermount).ok()?;
    if metadata.mode() & 0o4000 != 0 {
        return None;
    }
    Some(format!(
        "  {} is not setuid root (required for FUSE mounts).\n  \
         Fix: use the system fusermount3 instead:\n    \
         sudo apt install fuse3   # installs setuid /usr/bin/fusermount3",
        fusermount
    ))
}

fn user_namespace_hint() -> Option<String> {
    if read_sysctl("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref() == Some("1")
    {
        return Some(
            "  AppArmor is blocking unprivileged user namespaces.\n  \
             Fix:\n    \
             sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"
                .to_string(),
        );
    }
    if read_sysctl("/proc/sys/kernel/unprivileged_userns_clone").as_deref() == Some("0") {
        return Some(
            "  Unprivileged user namespaces are disabled.\n  \
             Fix:\n    \
             sudo sysctl -w kernel.unprivileged_userns_clone=1"
                .to_string(),
        );
    }
    None
}

fn read_sysctl(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::unescape_mount_point;

    #[test]
    fn mountinfo_escapes_are_decoded() {
        assert_eq!(
            unescape_mount_point(r"/home/a\040b/merged"),
            "/home/a b/merged"
        );
        assert_eq!(unescape_mount_point(r"/tab\011x"), "/tab\tx");
        assert_eq!(unescape_mount_point(r"/back\134slash"), "/back\\slash");
        assert_eq!(unescape_mount_point(r"/not\08escape"), r"/not\08escape");
        assert_eq!(unescape_mount_point("/plain"), "/plain");
    }
}
