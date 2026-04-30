//! The `forkspace` binary: parse arguments, dispatch, and map errors to exit codes.

use clap::Parser;
use owo_colors::OwoColorize;

use forkspace::cli::Cli;
use forkspace::commands;
use forkspace::error::ForkspaceError;

fn main() {
    // A re-executed copy of this binary may be a background manifest worker.
    if let Some(result) = forkspace::changes::manifest::run_worker_from_env() {
        if let Err(error) = result {
            exit_with(&error);
        }
        return;
    }
    if let Err(error) = commands::dispatch(Cli::parse()) {
        exit_with(&error);
    }
}

fn exit_with(error: &ForkspaceError) -> ! {
    eprintln!("{} {error}", "error:".red());
    std::process::exit(error.exit_code());
}
