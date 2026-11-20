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

    /// Why the candidates before the winner were passed over, e.g. `fuse-overlayfs: cannot open
    /// /dev/fuse: ...; reflinks: ext4 does not clone files (...)`; `None` when the first
    /// candidate won.
    pub fn skipped(&self) -> Option<String> {
        let skipped: Vec<String> = self
            .candidates
            .iter()
            .take_while(|candidate| !candidate.available)
            .map(|candidate| {
                let what = match candidate.backend {
                    BackendKind::Overlay => "fuse-overlayfs",
                    BackendKind::LinuxReflink => "reflinks",
                    BackendKind::MacClone => "APFS clones",
                    BackendKind::PlainCopy => "copy",
                };
                format!("{what}: {}", candidate.reason)
            })
            .collect();
        (!skipped.is_empty()).then(|| skipped.join("; "))
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

#[cfg(test)]
mod skipped_tests {
    use super::*;

    #[test]
    fn skipped_names_each_faster_candidate_and_its_reason() {
        let mut report = ProbeReport::default();
        report.record(
            BackendKind::Overlay,
            ProbeResult::unavailable("cannot open /dev/fuse: No such file or directory"),
        );
        report.record(
            BackendKind::LinuxReflink,
            ProbeResult::unavailable(
                "ext4 does not clone files (FICLONE: Operation not supported)",
            ),
        );
        report.record(BackendKind::PlainCopy, ProbeResult::available("always"));
        assert_eq!(
            report.skipped().as_deref(),
            Some(
                "fuse-overlayfs: cannot open /dev/fuse: No such file or directory; \
                 reflinks: ext4 does not clone files (FICLONE: Operation not supported)"
            )
        );
        let mut first_wins = ProbeReport::default();
        first_wins.record(BackendKind::Overlay, ProbeResult::available("usable"));
        assert_eq!(first_wins.skipped(), None);
    }
}
