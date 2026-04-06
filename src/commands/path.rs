use crate::error::ForkspaceError;
use crate::state::State;

pub fn run(name: String) -> Result<(), ForkspaceError> {
    let state = State::load()?;
    let ws = state.require_workspace(&name)?;

    // Print only the path, no trailing newline, no color — for scripting: $(forkspace path test)
    print!("{}", ws.merged.display());

    Ok(())
}
