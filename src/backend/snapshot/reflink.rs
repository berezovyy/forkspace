//! Linux reflink fork primitive: a tree copy whose regular files are cloned with the `FICLONE`
//! ioctl, falling back to a byte copy for any file the filesystem refuses to share.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::copy::materialize_tree;
use super::{ForkPrimitive, ForkStats, ForkSummary, DIRECTORY_COPY_CAPABILITIES};
use crate::backend::probe::ProbeResult;
use crate::backend::{BackendKind, Capabilities};
use crate::error::{ForkspaceError, Result};
use crate::util::{now_unix_ns, IoContext};

/// `_IOW(0x94, 9, int)` from `linux/fs.h`.
const FICLONE: libc::c_ulong = 0x4004_9409;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ReflinkPrimitive;

impl ForkPrimitive for ReflinkPrimitive {
    fn kind(&self) -> BackendKind {
        BackendKind::LinuxReflink
    }

    fn capabilities(&self) -> &'static Capabilities {
        &DIRECTORY_COPY_CAPABILITIES
    }

    fn check_available(&self, base: &Path, dest_parent: &Path) -> Result {
        let probe = probe(base, dest_parent);
        if probe.available {
            return Ok(());
        }
        Err(ForkspaceError::BackendUnavailable {
            diagnosis: probe.reason,
            remedy: "install fuse-overlayfs for O(1) forks".to_string(),
        })
    }

    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats> {
        materialize_tree(base, dest, reflink_file)
    }

    fn summarize(&self, stats: &ForkStats, _elapsed_ms: u128) -> ForkSummary {
        ForkSummary {
            degraded: false,
            note: Some(format!(
                "used reflinks for {} files ({} files required copying)",
                stats.reflinked_files, stats.copied_files
            )),
        }
    }
}

/// Clones `source` to a new `target`. Returns `false`, leaving no `target`, when the
/// filesystem refuses this particular file.
fn reflink_file(source: &Path, target: &Path) -> Result<bool> {
    let input =
        fs::File::open(source).io_context(|| format!("open {} for reflink", source.display()))?;
    let output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .io_context(|| format!("create {} for reflink", target.display()))?;
    if clone_file(&input, &output) {
        return Ok(true);
    }
    drop(output);
    fs::remove_file(target)
        .io_context(|| format!("reset {} after reflink refusal", target.display()))?;
    Ok(false)
}

/// Checks that `base` and `destination_parent` share a filesystem and that it accepts
/// `FICLONE`, by cloning a scratch file inside `destination_parent`.
pub(crate) fn probe(base: &Path, destination_parent: &Path) -> ProbeResult {
    if fs::create_dir_all(destination_parent).is_err() {
        return ProbeResult::unavailable("workspace root cannot be created");
    }
    let (Ok(base_meta), Ok(destination_meta)) =
        (fs::metadata(base), fs::metadata(destination_parent))
    else {
        return ProbeResult::unavailable("base or workspace root cannot be inspected");
    };
    if base_meta.dev() != destination_meta.dev() {
        return ProbeResult::unavailable("base and workspace root are on different filesystems");
    }
    if scratch_clone_succeeds(destination_parent) {
        ProbeResult::available("FICLONE scratch-file probe succeeded")
    } else {
        ProbeResult::unavailable("FICLONE scratch-file probe failed")
    }
}

fn scratch_clone_succeeds(directory: &Path) -> bool {
    let token = format!("{}-{}", std::process::id(), now_unix_ns());
    let source = directory.join(format!(".forkspace-reflink-source-{token}"));
    let target = directory.join(format!(".forkspace-reflink-target-{token}"));
    let cloned = (|| {
        fs::write(&source, b"x").ok()?;
        let input = fs::File::open(&source).ok()?;
        let output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)
            .ok()?;
        clone_file(&input, &output).then_some(())
    })()
    .is_some();
    let _ = fs::remove_file(&source);
    let _ = fs::remove_file(&target);
    cloned
}

fn clone_file(source: &fs::File, target: &fs::File) -> bool {
    // SAFETY: both descriptors are open for the duration of the call; FICLONE reads no memory.
    (unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) }) == 0
}
