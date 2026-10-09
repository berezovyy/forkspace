//! Background processes: dev servers, the proxy daemon in front of them, and liveness
//! tracking that survives PID reuse.

pub(crate) mod devserver;
pub mod members;
mod ports;
pub(crate) mod proxy;
mod spawn;
pub mod systemd;
pub mod watch;

use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};

/// Whether nothing is listening on `port` on either loopback address right now.
///
/// Dev servers that bind `localhost` may end up on `::1` alone (Node returns resolver results
/// unsorted since v17), so checking only `127.0.0.1` would hand out a port that is taken.
fn port_is_free(port: u16) -> bool {
    let ipv4_free = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok();
    ipv4_free
        && match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
            Ok(_) => true,
            // A host without IPv6 loopback cannot have anything listening there.
            Err(error) => error.kind() != std::io::ErrorKind::AddrInUse,
        }
}
