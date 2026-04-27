#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde::Serialize;

use super::cow::ProbeResult;
use super::BackendKind;

#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeVerdict {
    pub backend: BackendKind,
    pub available: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProbeReport {
    pub candidates: Vec<ProbeVerdict>,
}

impl ProbeReport {
    pub fn record(&mut self, backend: BackendKind, result: ProbeResult) {
        self.candidates.push(ProbeVerdict {
            backend,
            available: result.available,
            reason: result.reason,
        });
    }

    pub fn winner(&self) -> Option<BackendKind> {
        self.candidates
            .iter()
            .find(|candidate| candidate.available)
            .map(|candidate| candidate.backend)
    }

    pub fn winning_reason(&self) -> Option<&str> {
        self.candidates
            .iter()
            .find(|candidate| candidate.available)
            .map(|candidate| candidate.reason.as_str())
    }
}

#[cfg(target_os = "linux")]
pub fn linux_report(base: &Path, destination_parent: &Path) -> ProbeReport {
    let mut report = ProbeReport::default();
    report.record(
        BackendKind::Linux,
        match crate::overlay::probe_fuse_overlayfs() {
            Ok(()) => ProbeResult::available("fuse-overlayfs and /dev/fuse are usable"),
            Err(reason) => ProbeResult::unavailable(reason),
        },
    );
    report.record(
        BackendKind::LinuxReflink,
        reflink_probe(base, destination_parent),
    );
    report.record(
        BackendKind::PlainCopy,
        ProbeResult::available("portable directory copy is always available"),
    );
    report
}

pub fn reflink_supported(base: &Path, destination_parent: &Path) -> bool {
    reflink_probe(base, destination_parent).available
}

pub fn reflink_probe(base: &Path, destination_parent: &Path) -> ProbeResult {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (base, destination_parent);
        ProbeResult::unavailable("FICLONE is only available in Linux builds")
    }
    #[cfg(target_os = "linux")]
    {
        if fs::create_dir_all(destination_parent).is_err() {
            return ProbeResult::unavailable("workspace root cannot be created");
        }
        let (Ok(base_meta), Ok(destination_meta)) =
            (fs::metadata(base), fs::metadata(destination_parent))
        else {
            return ProbeResult::unavailable("base or workspace root cannot be inspected");
        };
        if base_meta.dev() != destination_meta.dev() {
            return ProbeResult::unavailable(
                "base and workspace root are on different filesystems",
            );
        }
        let token = format!("{}-{}", std::process::id(), crate::util::now_unix_ns());
        let source = destination_parent.join(format!(".forkspace-reflink-source-{token}"));
        let target = destination_parent.join(format!(".forkspace-reflink-target-{token}"));
        let result = (|| {
            fs::write(&source, b"x").ok()?;
            let source = fs::File::open(&source).ok()?;
            let target = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)
                .ok()?;
            clone_file(&source, &target).then_some(())
        })()
        .is_some();
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&target);
        if result {
            ProbeResult::available("FICLONE scratch-file probe succeeded")
        } else {
            ProbeResult::unavailable("FICLONE scratch-file probe failed")
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn clone_file(source: &fs::File, target: &fs::File) -> bool {
    (unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) }) == 0
}
