//! `forkspace run`: start (or `--stop`) a workspace's dev server behind the proxy.

use std::time::Duration;

use crate::cli::RunArgs;
use crate::error::{ForkspaceError, Result};
use crate::process::devserver::{self, PortDiscovery};
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
    })?;
    if args.port_wait > 0 {
        report_port(
            &args.name,
            devserver::discover_port(&args.name, Duration::from_secs(args.port_wait))?,
        );
    }
    Ok(())
}

fn report_port(name: &str, discovery: PortDiscovery) {
    match discovery {
        PortDiscovery::Assigned(port) => println!("listening on port {port}"),
        PortDiscovery::Moved { assigned, actual } => println!(
            "listening on port {actual} (it ignored $PORT={assigned}); the proxy forwards to {actual}"
        ),
        PortDiscovery::NotListening(port) => eprintln!(
            "note: nothing is listening yet; the proxy forwards to $PORT={port} \
             (`forkspace switch {name}` re-checks)"
        ),
        PortDiscovery::Exited { log_dir } => eprintln!(
            "warning: the command exited before listening; see {}",
            log_dir.display()
        ),
    }
}

fn stop(name: &str) -> Result {
    State::with_state(|state| {
        let process = devserver::stop(state, name)?;
        println!("stopped '{}' (PID {})", process.command, process.pid);
        Ok(())
    })
}
