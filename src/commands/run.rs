//! `forkspace run`: start (or `--stop`) a workspace's dev server behind the proxy.

use crate::cli::RunArgs;
use crate::error::{ForkspaceError, Result};
use crate::process::devserver;
use crate::state::State;

pub fn exec(args: RunArgs) -> Result {
    if args.stop {
        return stop(&args.name);
    }
    if args.command.is_empty() {
        return Err(ForkspaceError::InvalidArgument(
            "command required: forkspace run <workspace> -- <command>".to_string(),
        ));
    }

    let command = args.command.join(" ");
    State::with_state(|state| {
        let started = devserver::start(state, &args.name, &command, args.port)?;
        println!(
            "running '{}' on port {} (PID {})",
            command, started.process.port, started.process.pid
        );
        println!("proxy: http://localhost:{}", started.proxy_port);
        Ok(())
    })
}

fn stop(name: &str) -> Result {
    State::with_state(|state| {
        let process = devserver::stop(state, name)?;
        println!("stopped '{}' (PID {})", process.command, process.pid);
        Ok(())
    })
}
