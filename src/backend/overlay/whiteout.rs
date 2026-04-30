//! fuse-overlayfs whiteout markers: how the upper layer records deletions.

/// A whiteout entry found in an overlay upper directory.
pub(crate) enum Whiteout {
    /// `.wh..wh..opq`: the directory hides everything beneath it in the lower layer.
    Opaque,
    /// `.wh.<name>`: `<name>` was deleted.
    Deletion(String),
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
