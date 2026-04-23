use crate::cli::CompareArgs;
use crate::compare;
use crate::error::{ForkspaceError, Result};
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
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|error| ForkspaceError::Serialization(error.to_string()))?
        );
    } else if args.files {
        println!("{}", compare::render_files(&output));
    } else {
        println!("{}", compare::render(&output));
    }
    Ok(())
}
