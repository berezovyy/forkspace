//! The dev-server proxy: a detached daemon on a fixed port that forwards each TCP
//! connection to the dev server of the currently active workspace.
//!
//! The daemon holds no routing state of its own; it re-reads `state.json` per connection,
//! so `forkspace switch` takes effect without restarting it.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::Duration;

use super::port_is_free;
use super::spawn::{create_logs, spawn_detached};
use super::watch::{self, IDENTITY_ENV};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::state::{ProxyConfig, State};

/// Port the proxy daemon listens on.
const LISTEN_PORT: u16 = 4000;

/// Serve forever on `listen_port` on both loopback addresses; the body of the hidden
/// `proxy-daemon` subcommand. `127.0.0.1` is required; `::1` is added when the host has IPv6
/// loopback, since browsers and Node may resolve `localhost` to either.
pub fn run_proxy(listen_port: u16) -> Result {
    raise_open_file_limit();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, listen_port)).map_err(|e| {
        ForkspaceError::ProxyFailed(format!("failed to bind :{}: {}", listen_port, e))
    })?;
    eprintln!("proxy: listening on http://127.0.0.1:{}", listen_port);

    match TcpListener::bind((Ipv6Addr::LOCALHOST, listen_port)) {
        Ok(ipv6) => {
            eprintln!("proxy: listening on http://[::1]:{}", listen_port);
            thread::spawn(move || serve(ipv6));
        }
        Err(e) => eprintln!("proxy: not listening on [::1]:{listen_port}: {e}"),
    }
    serve(listener);
    Ok(())
}

fn serve(listener: TcpListener) {
    for client in listener.incoming() {
        match client {
            Ok(client) => {
                thread::spawn(move || forward_to_active(client));
            }
            // Out of descriptors: wait for open connections to close instead of spinning.
            Err(error) if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) => {
                thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {}
        }
    }
}

/// Each proxied connection holds four descriptors, and macOS starts processes with a soft
/// limit of 256, about 60 connections; raise it to the hard limit (at most `OPEN_MAX` on
/// macOS, which refuses more).
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid out-pointer for getrlimit, and setrlimit only reads it.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        let wanted = if cfg!(target_os = "macos") {
            limit.rlim_max.min(10_240)
        } else {
            limit.rlim_max
        };
        if limit.rlim_cur < wanted {
            limit.rlim_cur = wanted;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

/// Connect `client` to the active dev server; drop it if there is none or it is unreachable.
fn forward_to_active(client: TcpStream) {
    let Some(target_port) = active_target_port() else {
        return;
    };
    // The dev server may listen on either loopback address; try IPv4 first, then IPv6.
    let upstream_addresses = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, target_port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, target_port)),
    ];
    let Ok(upstream) = TcpStream::connect(&upstream_addresses[..]) else {
        return;
    };
    pipe(client, upstream);
}

fn active_target_port() -> Option<u16> {
    // `peek`: a connection should not start trash removals.
    State::peek()
        .ok()?
        .proxy?
        .target_port
        .filter(|&port| port != 0)
}

/// Copy bytes in both directions until each side reaches EOF, propagating each EOF as a
/// write half-close so request/response protocols see the end of the stream.
fn pipe(client: TcpStream, upstream: TcpStream) {
    let Ok(client_reader) = client.try_clone() else {
        return;
    };
    let Ok(upstream_reader) = upstream.try_clone() else {
        return;
    };
    let request = thread::spawn(move || copy_then_close(client_reader, upstream));
    copy_then_close(upstream_reader, client);
    request.join().ok();
}

fn copy_then_close(mut from: TcpStream, mut to: TcpStream) {
    io::copy(&mut from, &mut to).ok();
    to.shutdown(Shutdown::Write).ok();
}

/// The running proxy daemon's config, spawning the daemon first if it is not alive.
pub(crate) fn ensure_running(state: &mut State) -> Result<&mut ProxyConfig> {
    let proxy = match state.proxy.take() {
        Some(proxy) if daemon_alive(&proxy) => proxy,
        stale => {
            state.proxy = stale;
            spawn_daemon(LISTEN_PORT)?
        }
    };
    Ok(state.proxy.insert(proxy))
}

fn daemon_alive(proxy: &ProxyConfig) -> bool {
    proxy
        .proxy_pid
        .is_some_and(|pid| watch::is_alive(pid, proxy.proxy_identity.as_deref()))
}

fn spawn_daemon(listen_port: u16) -> Result<ProxyConfig> {
    if !port_is_free(listen_port) {
        return Err(ForkspaceError::ProxyFailed(format!(
            "port {listen_port} is unavailable; stop the process using it and retry"
        )));
    }

    let exe = std::env::current_exe()
        .map_err(|e| ForkspaceError::ProxyFailed(format!("cannot find self: {}", e)))?;

    let log_dir = paths::forkspace_home();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "warning: failed to create log directory {}: {e}",
            log_dir.display()
        );
    }
    let (stdout, stderr) = create_logs(&log_dir, "proxy.log", "proxy.err")
        .map_err(|e| ForkspaceError::ProxyFailed(format!("log file: {}", e)))?;

    let identity = watch::new_identity();
    let child = spawn_detached(
        Command::new(exe)
            .args(["proxy-daemon", "--port", &listen_port.to_string()])
            .env(IDENTITY_ENV, &identity)
            .stdout(stdout)
            .stderr(stderr),
    )
    .map_err(|e| ForkspaceError::ProxyFailed(format!("spawn proxy: {}", e)))?;

    // Give the daemon a moment to bind before the caller points a browser at it.
    thread::sleep(Duration::from_millis(100));

    Ok(ProxyConfig {
        listen_port,
        active_workspace: None,
        target_port: None,
        proxy_pid: Some(child.id()),
        proxy_identity: Some(watch::spawned_identity(child.id(), identity)),
        unknown_fields: serde_json::Map::new(),
    })
}
