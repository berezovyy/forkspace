//! `forkspace skill`: print the bundled instructions for coding agents.

use crate::error::Result;

/// The agent skill shipped with this version, in the `SKILL.md` format agents load.
pub const SKILL: &str = include_str!("skill.md");

pub fn exec() -> Result {
    print!("{SKILL}");
    Ok(())
}
