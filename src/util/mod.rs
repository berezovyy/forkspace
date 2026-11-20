//! Small, dependency-free helpers shared across modules.

pub(crate) mod bulk;
pub mod fs;
mod glob;
mod queue;
pub mod style;
pub mod walk;

pub use fs::{ensure_parent_dir, ensure_private_dir, write_atomic, IoContext, WalkDirExt};
pub use glob::glob_matches;

/// The current UTC time as an RFC 3339 timestamp (empty if formatting fails).
/// `1 file`, `3 files`: `n` with `noun`, plural unless `n` is one.
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// `45s`, `12m`, `5h`, `3d`.
pub fn describe_age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Seconds since the RFC 3339 time `at` (0 when it cannot be read).
pub fn seconds_since(at: &str) -> u64 {
    time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
        .map(|then| {
            (time::OffsetDateTime::now_utc() - then)
                .whole_seconds()
                .max(0) as u64
        })
        .unwrap_or(0)
}

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
