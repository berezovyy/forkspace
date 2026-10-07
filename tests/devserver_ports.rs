#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use forkspace::state::State;
use serial_test::serial;

/// Listens on `[::1]:6311` whatever `$PORT` says, like a dev server bound to `localhost`
/// on a host that resolves it to IPv6.
const IPV6_ONLY_SERVER: &str = r#"
import http.server, socket
class Server(http.server.ThreadingHTTPServer):
    address_family = socket.AF_INET6
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"ipv6-only"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
Server(("::1", 6311), Handler).serve_forever()
"#;

fn forkspace(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home)
        .args(args)
        .output()
        .unwrap()
}

fn get(address: SocketAddr) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response.lines().last().map(str::to_string)
}

fn ipv6_loopback_available() -> bool {
    std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

#[test]
#[serial]
fn run_follows_a_server_that_ignores_port_and_listens_on_ipv6_only() {
    if !ipv6_loopback_available()
        || Command::new("python3").arg("--version").output().is_err()
        || std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 4000)).is_err()
    {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"ipv6 loopback, python3, free port 4000\"}}"
        );
        return;
    }
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("server.py"), IPV6_ONLY_SERVER).unwrap();

    let created = forkspace(
        home.path(),
        &[
            "new",
            "ports",
            "--from",
            base.path().to_str().unwrap(),
            "--copy",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let run = forkspace(
        home.path(),
        &[
            "run",
            "ports",
            "--port",
            "6300",
            "--",
            "python3",
            "server.py",
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("listening on port 6311 (it ignored $PORT=6300)"),
        "{stdout}"
    );

    let state = State::load().unwrap();
    assert_eq!(
        state.workspaces["ports"].process.as_ref().unwrap().port,
        6311
    );
    let proxy = state.proxy.clone().unwrap();
    assert_eq!(proxy.target_port, Some(6311));

    let deadline = Instant::now() + Duration::from_secs(10);
    for address in [
        SocketAddr::from((Ipv4Addr::LOCALHOST, 4000)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, 4000)),
    ] {
        while get(address).as_deref() != Some("ipv6-only") {
            assert!(
                Instant::now() < deadline,
                "proxy on {address} never answered"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    assert!(forkspace(home.path(), &["run", "ports", "--stop"])
        .status
        .success());
    assert!(forkspace::process::watch::terminate_group(
        proxy.proxy_pid.unwrap(),
        proxy.proxy_identity.as_deref(),
    ));
}
