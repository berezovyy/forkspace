//! Fork-time capability probes: which materialization primitives work for a given base and
//! workspace root, and why.

use std::path::Path;

use serde::Serialize;

use super::BackendKind;

/// Whether one primitive is usable, with a human-readable reason either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub available: bool,
    pub reason: String,
}

impl ProbeResult {
    pub fn available(reason: impl Into<String>) -> Self {
        Self {
            available: true,
            reason: reason.into(),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeVerdict {
    pub backend: BackendKind,
    pub available: bool,
    pub reason: String,
}

/// Probe verdicts in preference order; the first available candidate wins.
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
        self.first_available().map(|candidate| candidate.backend)
    }

    pub fn winning_reason(&self) -> Option<&str> {
        self.first_available()
            .map(|candidate| candidate.reason.as_str())
    }

    fn first_available(&self) -> Option<&ProbeVerdict> {
        self.candidates.iter().find(|candidate| candidate.available)
    }
}

/// Probes the Linux primitives in preference order: overlay, reflink, then plain copy.
#[cfg(target_os = "linux")]
pub fn linux_report(base: &Path, destination_parent: &Path) -> ProbeReport {
    let mut report = ProbeReport::default();
    report.record(
        BackendKind::Overlay,
        match super::overlay::mount::probe_fuse_overlayfs() {
            Ok(()) => ProbeResult::available("fuse-overlayfs and /dev/fuse are usable"),
            Err(reason) => ProbeResult::unavailable(reason),
        },
    );
    report.record(
        BackendKind::LinuxReflink,
        super::snapshot::reflink_probe(base, destination_parent),
    );
    report.record(
        BackendKind::PlainCopy,
        ProbeResult::available("portable directory copy is always available"),
    );
    report
}

/// Whether files in `base` can be reflinked into `destination_parent`, by actually cloning a
/// scratch file with the FICLONE ioctl (filesystem names are not trusted).
#[cfg(target_os = "linux")]
pub fn reflink_supported(base: &Path, destination_parent: &Path) -> bool {
    super::snapshot::reflink_probe(base, destination_parent).available
}

/// Whether files in `base` can be reflinked into `destination_parent`; FICLONE is Linux-only.
#[cfg(not(target_os = "linux"))]
pub fn reflink_supported(_base: &Path, _destination_parent: &Path) -> bool {
    false
}
