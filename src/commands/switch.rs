//! `forkspace switch`: point the proxy at another workspace's running dev server.

use crate::cli::SwitchArgs;
use crate::error::Result;
use crate::process::devserver;
use crate::state::State;

pub fn exec(args: SwitchArgs) -> Result {
    State::with_state(|state| {
        let port = devserver::switch_to(state, &args.name)?;
        println!("switched to '{}' (port {})", args.name, port);
        Ok(())
    })
}
