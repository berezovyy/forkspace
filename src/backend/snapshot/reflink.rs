#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;

use super::copy::materialize_tree;
use super::{Degradation, ForkPrimitive, ForkStats, ForkTimePolicy, ProbeResult};
use crate::backend::{probe, BackendKind};
use crate::error::Result;
use crate::util::IoContext;

#[derive(Debug, Clone, Copy, Default)]
pub struct ReflinkPrimitive;

impl ForkPrimitive for ReflinkPrimitive {
    fn kind(&self) -> BackendKind {
        BackendKind::LinuxReflink
    }

    fn probe(&self, base: &Path, dest_parent: &Path) -> ProbeResult {
        probe::reflink_probe(base, dest_parent)
    }

    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats> {
        materialize_tree(base, dest, |source, target| {
            let input = fs::File::open(source)
                .io_context(|| format!("open {} for reflink", source.display()))?;
            let output = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(target)
                .io_context(|| format!("create {} for reflink", target.display()))?;
            if probe::clone_file(&input, &output) {
                Ok(true)
            } else {
                drop(output);
                fs::remove_file(target)
                    .io_context(|| format!("reset {} after reflink refusal", target.display()))?;
                Ok(false)
            }
        })
    }

    fn fork_time_policy(&self) -> ForkTimePolicy {
        ForkTimePolicy::AfterMaterialize
    }

    fn degradation(&self) -> Option<Degradation> {
        Some(Degradation {
            diagnosis: "fuse-overlayfs is unavailable; forkspace used per-file reflinks".to_string(),
            remedy: "install fuse-overlayfs for O(1) forks".to_string(),
        })
    }
}
