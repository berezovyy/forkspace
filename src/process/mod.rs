//! Background processes: dev servers, the proxy daemon in front of them, and liveness
//! tracking that survives PID reuse.

pub(crate) mod devserver;
pub mod members;
mod ports;
pub(crate) mod proxy;
pub mod sandbox;
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
        && wildcard_is_free(port)
}

/// Whether nothing holds `port` on all addresses. macOS lets a loopback bind with
/// `SO_REUSEADDR` (which std sets) succeed next to a `*:port` listener, such as AirPlay
/// Receiver's on 5000 and 7000 or a port Docker publishes; only a wildcard bind sees it. The
/// socket is bound but never listens, so no firewall prompt appears.
#[cfg(target_os = "macos")]
fn wildcard_is_free(port: u16) -> bool {
    let bind = |family: libc::c_int| -> bool {
        // SAFETY: plain socket calls on a descriptor this function owns and closes; the
        // address structs are zeroed and then filled in.
        unsafe {
            let socket = libc::socket(family, libc::SOCK_STREAM, 0);
            if socket < 0 {
                return true;
            }
            let on: libc::c_int = 1;
            let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            libc::setsockopt(
                socket,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&on as *const libc::c_int).cast(),
                size,
            );
            let status = if family == libc::AF_INET6 {
                libc::setsockopt(
                    socket,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&on as *const libc::c_int).cast(),
                    size,
                );
                let mut address: libc::sockaddr_in6 = std::mem::zeroed();
                address.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
                address.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                address.sin6_port = port.to_be();
                libc::bind(
                    socket,
                    (&address as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            } else {
                let mut address: libc::sockaddr_in = std::mem::zeroed();
                address.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
                address.sin_family = libc::AF_INET as libc::sa_family_t;
                address.sin_port = port.to_be();
                libc::bind(
                    socket,
                    (&address as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            let in_use = status != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EADDRINUSE);
            libc::close(socket);
            !in_use
        }
    };
    bind(libc::AF_INET) && bind(libc::AF_INET6)
}

/// Linux refuses a loopback bind next to a wildcard listener, so the loopback checks suffice.
#[cfg(not(target_os = "macos"))]
fn wildcard_is_free(_port: u16) -> bool {
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_port_held_on_all_addresses_is_not_free() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!super::port_is_free(port));
        drop(listener);
    }
}
