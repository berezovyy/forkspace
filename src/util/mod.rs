//! Small, dependency-free helpers shared across modules.

pub mod fs;
mod glob;

pub use fs::{ensure_parent_dir, write_atomic, IoContext, WalkDirExt};
pub use glob::glob_matches;

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

pub fn now_unix_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
