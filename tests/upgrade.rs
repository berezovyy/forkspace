//! `forkspace upgrade` against a release laid out like GitHub's, served from `file://`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A release directory holding `download/v<version>/<asset>` and its checksum, where the
/// "binary" is a script that reports `version`.
fn publish(releases: &Path, version: &str, checksum: Option<&str>) {
    let target = forkspace::commands::upgrade::release_target().unwrap();
    let staging = releases.join("staging");
    fs::create_dir_all(&staging).unwrap();
    let binary = staging.join("forkspace");
    fs::write(&binary, format!("#!/bin/sh\necho 'forkspace {version}'\n")).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

    let dir = releases.join(format!("download/v{version}"));
    fs::create_dir_all(&dir).unwrap();
    let asset = format!("forkspace-{version}-{target}.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(dir.join(&asset))
        .arg("-C")
        .arg(&staging)
        .arg("forkspace")
        .status()
        .unwrap();
    assert!(status.success());
    let sum = match checksum {
        Some(sum) => sum.to_string(),
        None => {
            // macOS has shasum but not always sha256sum.
            let output = Command::new("shasum")
                .args(["-a", "256"])
                .arg(dir.join(&asset))
                .output()
                .or_else(|_| Command::new("sha256sum").arg(dir.join(&asset)).output())
                .unwrap();
            String::from_utf8(output.stdout)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        }
    };
    fs::write(
        dir.join(format!("{asset}.sha256")),
        format!("{sum}  {asset}\n"),
    )
    .unwrap();
}

/// A copy of the real binary in its own directory, so the upgrade replaces the copy.
fn installed_copy(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("forkspace");
    fs::copy(env!("CARGO_BIN_EXE_forkspace"), &exe).unwrap();
    exe
}

/// Run a freshly written binary. A test on another thread that forks while the copy is open
/// for writing holds that descriptor until its exec, and exec then fails with ETXTBSY.
fn output(command: &mut Command) -> Output {
    for _ in 0..50 {
        match command.output() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            result => return result.unwrap(),
        }
    }
    panic!("{command:?} stayed busy");
}

fn upgrade(exe: &Path, releases: &Path, args: &[&str]) -> Output {
    output(
        Command::new(exe)
            .arg("upgrade")
            .args(args)
            .env(
                "FORKSPACE_RELEASES_URL",
                format!("file://{}", releases.display()),
            )
            .env("CARGO_HOME", releases.join("no-cargo-home")),
    )
}

fn version_of(exe: &Path) -> String {
    let output = output(Command::new(exe).arg("--version"));
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn upgrade_replaces_the_binary_with_a_verified_release() {
    let root = tempfile::tempdir().unwrap();
    let releases = root.path().join("releases");
    publish(&releases, "9.9.9", None);
    let exe = installed_copy(root.path());

    let output = upgrade(&exe, &releases, &["9.9.9"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(version_of(&exe), "forkspace 9.9.9");
    let leftovers: Vec<_> = fs::read_dir(exe.parent().unwrap()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "staging file left beside the binary");
}

#[test]
fn upgrade_refuses_a_download_whose_checksum_does_not_match() {
    let root = tempfile::tempdir().unwrap();
    let releases = root.path().join("releases");
    publish(&releases, "9.9.9", Some(&"0".repeat(64)));
    let exe = installed_copy(root.path());
    let before = version_of(&exe);

    let output = upgrade(&exe, &releases, &["9.9.9"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"));
    assert_eq!(version_of(&exe), before);
}

#[test]
fn upgrade_to_the_running_version_does_nothing() {
    let root = tempfile::tempdir().unwrap();
    let exe = installed_copy(root.path());
    let current = env!("CARGO_PKG_VERSION");

    let output = upgrade(&exe, &root.path().join("missing"), &[current]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("up to date"));
}
