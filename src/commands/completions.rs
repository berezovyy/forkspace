//! `forkspace completions <shell>`: print a shell completion script.
//!
//! zsh gets a hand-written script so workspace arguments complete to live workspace names
//! (via `forkspace ls --names`); other shells use `clap_complete`'s static generator.

use std::io;

use clap::CommandFactory;
use clap_complete::{generate, Shell};

use crate::cli::Cli;
use crate::error::Result;

const ZSH_COMPLETIONS: &str = include_str!("completions.zsh");

pub fn exec(shell: Shell) -> Result {
    match shell {
        Shell::Zsh => print!("{}", ZSH_COMPLETIONS),
        _ => generate(shell, &mut Cli::command(), "forkspace", &mut io::stdout()),
    }
    Ok(())
}
