use crate::cli::SwitchArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::util::is_pid_alive;

pub fn exec(args: SwitchArgs) -> Result {
    State::with_state(|state| {
        let ws = state.require_workspace(&args.name)?;

        let proc = ws
            .process
            .as_ref()
            .ok_or_else(|| ForkspaceError::NoProcessRunning(args.name.clone()))?;

        if !is_pid_alive(proc.pid) {
            return Err(ForkspaceError::NoProcessRunning(args.name.clone()));
        }

        let port = proc.port;

        if let Some(ref mut proxy) = state.proxy {
            proxy.active_workspace = Some(args.name.clone());
            proxy.target_port = Some(port);
            println!("switched to '{}' (port {})", args.name, port);
        } else {
            return Err(ForkspaceError::ProxyFailed(
                "no proxy running — start a process first with `forkspace run`".to_string(),
            ));
        }

        Ok(())
    })
}
