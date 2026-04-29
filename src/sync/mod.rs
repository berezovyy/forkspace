//! Moving changes between a workspace and its base.
//!
//! [`plan`] decides what to transfer and detects conflicts, [`journal`] applies the transfer
//! as resumable per-path steps, and [`merge`] / [`pull`] are the two directions.

pub mod journal;
pub mod merge;
pub mod plan;
pub mod pull;
