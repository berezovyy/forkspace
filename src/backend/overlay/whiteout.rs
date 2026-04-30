//! fuse-overlayfs whiteout markers: how the upper layer records deletions.

use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

/// A whiteout entry found in an overlay upper directory.
pub(crate) enum Whiteout {
    /// `.wh..wh..opq`: the directory hides everything beneath it in the lower layer.
    Opaque,
    /// `.wh.<name>`: `<name>` was deleted.
    Deletion(String),
}

/// Whether `metadata` is a kernel-style whiteout: a 0:0 character device with the deleted
/// entry's own name. fuse-overlayfs writes these when it can create device nodes and falls
/// back to `.wh.<name>` files when it cannot.
pub(crate) fn is_device_whiteout(metadata: &Metadata) -> bool {
    metadata.file_type().is_char_device() && metadata.rdev() == 0
}

impl Whiteout {
    pub(crate) fn parse(filename: &str) -> Option<Whiteout> {
        if filename == ".wh..wh..opq" {
            return Some(Whiteout::Opaque);
        }
        filename
            .strip_prefix(".wh.")
            .map(|name| Whiteout::Deletion(name.to_string()))
    }
}
