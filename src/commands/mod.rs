//! One handler per subcommand: check the arguments, call into the domain modules, and
//! print the result.

pub(crate) mod checkpoint;
pub(crate) mod compare;
pub(crate) mod completions;
pub mod diff;
pub mod drop;
pub(crate) mod enter;
pub mod land;
pub mod ls;
pub(crate) mod merge;
pub mod new;
pub(crate) mod pull;
pub(crate) mod run;
pub(crate) mod switch;

use crate::cli::{Cli, Commands};
use crate::error::Result;

/// Print `value` as pretty JSON on stdout (the `--json` output of every command).
fn print_json(value: &impl serde::Serialize) -> Result {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Run the parsed subcommand.
pub fn dispatch(cli: Cli) -> Result {
    match cli.command {
        Commands::New(args) => new::exec(args),
        Commands::Drop(args) => drop::exec(args),
        Commands::Enter(args) => enter::exec(args),
        Commands::Run(args) => run::exec(args),
        Commands::Switch(args) => switch::exec(args),
        Commands::Ls(args) => ls::exec(args),
        Commands::Completions(args) => completions::exec(args.shell),
        Commands::Diff(args) => diff::exec(args),
        Commands::Compare(args) => compare::exec(args),
        Commands::Merge(args) => merge::exec(args),
        Commands::Pull(args) => pull::exec(args),
        Commands::Land(args) => land::exec(args),
        Commands::Checkpoint(args) => checkpoint::save(args),
        Commands::Checkpoints(args) => checkpoint::list(args),
        Commands::Restore(args) => checkpoint::restore(args),
        Commands::ProxyDaemon(args) => crate::process::proxy::run_proxy(args.port),
    }
}
