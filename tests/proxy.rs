//! The dev-server proxy forwards `localhost:4000` to the running dev server, and exits once
//! no dev server is left, so the port is not held forever.

#![cfg(target_os = "linux")]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn port_4000_open() -> bool {
    TcpStream::connect(("127.0.0.1", 4000)).is_ok()
}

#[test]
fn the_proxy_serves_the_dev_server_and_exits_when_it_stops() {
    if port_4000_open() {
        eprintln!("skipping: port 4000 is already in use");
        return;
    }
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let home = common::TestHome::new();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("index.html"),
        "hello from the workspace\n",
    )
    .unwrap();
    let forkspace = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .env("FORKSPACE_NO_SYSTEMD", "1")
            .args(args)
            .output()
            .unwrap()
    };
    let project_path = project.path().to_str().unwrap();
    assert!(forkspace(&["new", "web", "--from", project_path, "--copy"])
        .status
        .success());
    let started = forkspace(&[
        "run",
        "web",
        "--",
        "python3",
        "-m",
        "http.server",
        "$PORT",
        "--bind",
        "127.0.0.1",
    ]);
    assert!(
        String::from_utf8_lossy(&started.stdout).contains("listening on port"),
        "{started:?}"
    );

    let mut stream = TcpStream::connect(("127.0.0.1", 4000)).unwrap();
    stream
        .write_all(b"GET /index.html HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.contains("hello from the workspace"), "{response}");

    assert!(forkspace(&["run", "web", "--stop"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    while port_4000_open() {
        assert!(
            Instant::now() < deadline,
            "the proxy kept port 4000 after the server stopped"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(forkspace(&["drop", "web"]).status.success());
}
