//! Change detection: what differs between a workspace view and its base.
//!
//! Overlay workspaces are scanned from their upper directory ([`overlay`]); directory
//! snapshots are compared against the fork manifest ([`snapshot`]). Both produce the same
//! [`FileChange`] list, which [`render`] turns into CLI output.

mod content;
mod cumulative;
pub mod filter;
pub mod manifest;
mod overlay;
pub mod render;
mod snapshot;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use filter::{ChangeClass, ChangeFilter};

pub use content::is_binary;
pub use cumulative::{collect_cumulative_changes, collect_cumulative_changes_fast};
pub use overlay::{collect_changes, collect_changes_fast};
pub use render::{format_diff, generate_unified_diff, DiffFormat, DiffOutput};
pub use snapshot::collect_clone_changes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    #[serde(with = "path_serde")]
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub class: ChangeClass,
    pub additions: Option<usize>,
    pub deletions: Option<usize>,
    pub is_binary: bool,
    #[serde(skip, default)]
    pub old_content: OldContent,
}

mod path_serde {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    const BYTE_PREFIX: &str = "\0forkspace-bytes:";

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(path) = path.to_str() {
            return serializer.serialize_str(path);
        }
        let mut encoded = String::from(BYTE_PREFIX);
        for byte in path.as_os_str().as_bytes() {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
        }
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let Some(hex) = encoded.strip_prefix(BYTE_PREFIX) else {
            return Ok(PathBuf::from(encoded));
        };
        if hex.len() % 2 != 0 {
            return Err(serde::de::Error::custom("invalid encoded path length"));
        }
        let bytes = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let value = std::str::from_utf8(pair)
                    .ok()
                    .and_then(|value| u8::from_str_radix(value, 16).ok());
                value.ok_or_else(|| serde::de::Error::custom("invalid encoded path byte"))
            })
            .collect::<Result<Vec<_>, D::Error>>()?;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OldContent {
    #[default]
    Available,
    BaseAdvanced,
    AncestorUnknown,
}

impl FileChange {
    pub(crate) fn deletion(path: PathBuf) -> Self {
        Self {
            class: ChangeFilter::class(&path),
            path,
            kind: ChangeKind::Deleted,
            additions: None,
            deletions: None,
            is_binary: false,
            old_content: OldContent::Available,
        }
    }
}
