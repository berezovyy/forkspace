//! `forkspace compare`: side-by-side facts about sibling workspaces of one base.

use crate::cli::CompareArgs;
use crate::error::Result;
use crate::review::compare;
use crate::state::State;

pub fn exec(args: CompareArgs) -> Result {
    let state = State::load()?;
    let workspaces = args
        .names
        .iter()
        .map(|name| state.require_workspace(name).cloned())
        .collect::<Result<Vec<_>>>()?;
    let output = compare::build(&workspaces)?;
    if args.json {
        super::print_json(&output)?;
    } else if args.files {
        println!("{}", compare::render_files(&output));
    } else {
        println!("{}", compare::render(&output));
    }
    Ok(())
}
