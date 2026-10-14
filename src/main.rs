//! The `forkspace` binary: parse arguments, dispatch, and map errors to exit codes.

use clap::Parser;

use forkspace::cli::Cli;
use forkspace::commands;
use forkspace::error::ForkspaceError;
use forkspace::util::style::{paint_stderr, Style};

fn main() {
    // A re-executed copy of this binary may be a background manifest worker.
    if let Some(result) = forkspace::changes::manifest::run_worker_from_env() {
        if let Err(error) = result {
            exit_with(&error);
        }
        return;
    }
    let result = forkspace::util::trace::time("command", || commands::dispatch(Cli::parse()));
    if let Err(error) = result {
        exit_with(&error);
    }
}

fn exit_with(error: &ForkspaceError) -> ! {
    eprintln!("{} {error}", paint_stderr("error:", Style::Red));
    std::process::exit(error.exit_code());
}
