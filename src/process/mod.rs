//! Background processes: dev servers, the proxy daemon in front of them, and liveness
//! tracking that survives PID reuse.

pub(crate) mod devserver;
pub(crate) mod proxy;
mod spawn;
pub mod watch;

/// Whether nothing is listening on `127.0.0.1:port` right now.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}
