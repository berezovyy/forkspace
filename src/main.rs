use clap::Parser;
use owo_colors::OwoColorize;

use forkspace::cli::Cli;
use forkspace::commands;

fn main() {
    if let Some(result) = forkspace::manifest::run_worker_from_env() {
        if let Err(error) = result {
            eprintln!("{} {error}", "error:".red());
            std::process::exit(error.exit_code());
        }
        return;
    }
    let cli = Cli::parse();
    if let Err(e) = commands::dispatch(cli) {
        eprintln!("{} {e}", "error:".red());
        std::process::exit(e.exit_code());
    }
}
