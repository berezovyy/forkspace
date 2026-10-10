//! Small, dependency-free helpers shared across modules.

pub mod fs;
mod glob;
pub mod trace;
pub mod walk;

pub use fs::{ensure_parent_dir, ensure_private_dir, write_atomic, IoContext, WalkDirExt};
pub use glob::glob_matches;

/// The current UTC time as an RFC 3339 timestamp (empty if formatting fails).
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Nanoseconds since the Unix epoch (0 if the clock is before it).
pub fn now_unix_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
