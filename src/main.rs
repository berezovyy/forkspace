use clap::Parser;
use owo_colors::OwoColorize;

use forkspace::cli::Cli;
use forkspace::commands;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = commands::dispatch(cli) {
        eprintln!("{} {e}", "error:".red());
        std::process::exit(e.exit_code());
    }
}
