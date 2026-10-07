//! Opt-in phase timings for benchmarks and slow-command reports.
//!
//! With `FORKSPACE_TRACE=1` in the environment, every timed phase prints one line to stderr
//! when it ends: `forkspace-trace: <phase> <milliseconds>ms`. Stdout is never written, so the
//! CLI's output contract is unchanged. Without the variable a span costs one cached check.

use std::sync::OnceLock;
use std::time::Instant;

/// The stderr line prefix that marks a trace line.
pub const PREFIX: &str = "forkspace-trace:";

/// Whether `FORKSPACE_TRACE` is set to something other than empty or `0`.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("FORKSPACE_TRACE").is_some_and(|value| !value.is_empty() && value != "0")
    })
}

/// A running phase; its duration is printed when it is dropped.
#[must_use = "a span measures until it is dropped"]
pub struct Span {
    phase: &'static str,
    started: Option<Instant>,
}

/// Start timing `phase` (a no-op unless tracing is enabled).
pub fn span(phase: &'static str) -> Span {
    Span {
        phase,
        started: enabled().then(Instant::now),
    }
}

/// Run `f` as the phase `phase`.
pub fn time<T>(phase: &'static str, f: impl FnOnce() -> T) -> T {
    let _span = span(phase);
    f()
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            eprintln!("{PREFIX} {} {ms:.3}ms", self.phase);
        }
    }
}
