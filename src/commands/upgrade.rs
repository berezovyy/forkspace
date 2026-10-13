//! `forkspace upgrade`: replace this binary with another release from GitHub.
//!
//! Releases carry one `forkspace-<version>-<target>.tar.gz` per platform, each with a
//! `.sha256` beside it. A binary installed by `cargo install` is upgraded by Cargo instead, and
//! a platform without a prebuilt binary falls back to Cargo when it is on `PATH`. Downloads go
//! through `curl`, so the upgrade adds no HTTP client to the binary.
//!
//! `FORKSPACE_RELEASES_URL` replaces `<repository>/releases` as the place releases are fetched
//! from; the tests point it at a `file://` directory.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::UpgradeArgs;
use crate::error::{ForkspaceError, Result};
use crate::util::fs::IoContext;

const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub fn exec(args: UpgradeArgs) -> Result {
    let base = releases_url();
    let explicit = args.version.is_some();
    let target_version = match args.version {
        Some(version) => version.trim_start_matches('v').to_string(),
        None => latest_version(&base)?,
    };
    if parse_version(&target_version).is_none() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "'{target_version}' is not a version; expected something like 0.2.0"
        )));
    }

    if args.check {
        if newer(&target_version, CURRENT) {
            println!(
                "forkspace {target_version} is available (you have {CURRENT}); run `forkspace upgrade`"
            );
        } else {
            println!("forkspace {CURRENT} is up to date (latest release: {target_version})");
        }
        return Ok(());
    }
    if target_version == CURRENT || (!explicit && !newer(&target_version, CURRENT)) {
        println!("forkspace {CURRENT} is up to date (latest release: {target_version})");
        return Ok(());
    }

    let exe = env::current_exe()
        .and_then(fs::canonicalize)
        .io_context(|| "failed to locate the running forkspace binary".to_string())?;
    if exe.components().any(|part| part.as_os_str() == "Cellar") {
        return Err(ForkspaceError::InvalidArgument(
            "this forkspace was installed with Homebrew; upgrade it with `brew upgrade forkspace`"
                .to_string(),
        ));
    }
    if installed_by_cargo(&exe) {
        return cargo_install(&target_version);
    }

    let target = release_target()?;
    println!("forkspace {CURRENT} -> {target_version} ({target})");
    match download_binary(&base, &target_version, &target, &exe)? {
        Downloaded::Installed => {}
        Downloaded::NoAsset if which("cargo") => {
            println!("no prebuilt binary for {target} in {target_version}; building it with Cargo");
            return cargo_install(&target_version);
        }
        Downloaded::NoAsset => {
            return Err(ForkspaceError::InvalidArgument(format!(
                "release {target_version} has no prebuilt binary for {target}; install it with \
                 `cargo install forkspace --version {target_version}`"
            )));
        }
    }

    let reported = Command::new(&exe)
        .arg("--version")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if reported != format!("forkspace {target_version}") {
        eprintln!(
            "warning: the new binary reports '{reported}', expected forkspace {target_version}"
        );
    }
    println!("upgraded {} to {target_version}", exe.display());
    Ok(())
}

/// The release asset suffix for this machine. Linux binaries are static (musl), so one build
/// runs on every distribution.
pub fn release_target() -> Result<String> {
    let arch = match env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            return Err(ForkspaceError::InvalidArgument(format!(
                "no prebuilt forkspace binaries for {other}; install with `cargo install forkspace`"
            )))
        }
    };
    let os = match env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        other => {
            return Err(ForkspaceError::InvalidArgument(format!(
                "no prebuilt forkspace binaries for {other}"
            )))
        }
    };
    Ok(format!("{arch}-{os}"))
}

fn releases_url() -> String {
    env::var("FORKSPACE_RELEASES_URL")
        .unwrap_or_else(|_| format!("{}/releases", env!("CARGO_PKG_REPOSITORY")))
        .trim_end_matches('/')
        .to_string()
}

/// GitHub redirects `releases/latest` to `releases/tag/<tag>`; reading the redirect needs no
/// API token and has no rate limit.
fn latest_version(base: &str) -> Result<String> {
    let output = Command::new("curl")
        .args(["-fsSL", "-o", "/dev/null", "-w", "%{url_effective}"])
        .arg(format!("{base}/latest"))
        .output()
        .map_err(|error| curl_missing(&error))?;
    if !output.status.success() {
        return Err(ForkspaceError::ProcessFailed(format!(
            "could not reach {base}/latest: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let url = String::from_utf8_lossy(&output.stdout);
    tag_from_url(&url).ok_or_else(|| {
        ForkspaceError::ProcessFailed(format!("no forkspace release is published at {base}"))
    })
}

fn tag_from_url(url: &str) -> Option<String> {
    let (_, tag) = url.trim().rsplit_once("/tag/")?;
    let version = tag.trim_start_matches('v');
    parse_version(version).map(|_| version.to_string())
}

enum Downloaded {
    Installed,
    NoAsset,
}

fn download_binary(base: &str, version: &str, target: &str, exe: &Path) -> Result<Downloaded> {
    let asset = format!("forkspace-{version}-{target}.tar.gz");
    let url = format!("{base}/download/v{version}/{asset}");
    let scratch = env::temp_dir().join(format!("forkspace-upgrade-{}", std::process::id()));
    fs::create_dir_all(&scratch)
        .io_context(|| format!("failed to create {}", scratch.display()))?;
    let result = (|| {
        let archive = scratch.join(&asset);
        if !fetch(&url, &archive)? {
            return Ok(Downloaded::NoAsset);
        }
        let sums = scratch.join(format!("{asset}.sha256"));
        if !fetch(&format!("{url}.sha256"), &sums)? {
            return Err(ForkspaceError::ProcessFailed(format!(
                "{asset} has no published checksum; refusing to install it"
            )));
        }
        let expected = fs::read_to_string(&sums)
            .io_context(|| format!("failed to read {}", sums.display()))?;
        let expected = expected.split_whitespace().next().unwrap_or_default();
        let actual = sha256(&archive)?;
        if !expected.eq_ignore_ascii_case(&actual) {
            return Err(ForkspaceError::ProcessFailed(format!(
                "checksum mismatch for {asset}: expected {expected}, got {actual}"
            )));
        }
        run(Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&scratch))?;
        replace_binary(&scratch.join("forkspace"), exe)?;
        Ok(Downloaded::Installed)
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// Download `url` to `to`. `Ok(false)` means the server has no such file.
fn fetch(url: &str, to: &Path) -> Result<bool> {
    let output = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(to)
        .arg(url)
        .output()
        .map_err(|error| curl_missing(&error))?;
    match output.status.code() {
        Some(0) => Ok(true),
        // 22: HTTP 4xx (GitHub answers 404 for a missing asset); 37: no such local file.
        Some(22) | Some(37) => Ok(false),
        _ => Err(ForkspaceError::ProcessFailed(format!(
            "failed to download {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

fn sha256(path: &Path) -> Result<String> {
    for tool in [&["sha256sum"][..], &["shasum", "-a", "256"][..]] {
        let Ok(output) = Command::new(tool[0]).args(&tool[1..]).arg(path).output() else {
            continue;
        };
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            return Ok(text
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string());
        }
    }
    Err(ForkspaceError::ProcessFailed(
        "neither sha256sum nor shasum is available to verify the download".to_string(),
    ))
}

/// Move `new` over `exe` atomically: stage it beside `exe` (same filesystem), then rename. A
/// running process keeps the old file open, so replacing it while forkspace runs is safe.
fn replace_binary(new: &Path, exe: &Path) -> Result {
    if !new.is_file() {
        return Err(ForkspaceError::ProcessFailed(
            "the release archive has no forkspace binary".to_string(),
        ));
    }
    let dir = exe.parent().unwrap_or(Path::new("/"));
    let staged = dir.join(format!(".forkspace-upgrade-{}", std::process::id()));
    if let Err(error) = fs::copy(new, &staged) {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(ForkspaceError::InvalidArgument(format!(
                "no permission to replace {}; rerun with `sudo forkspace upgrade`",
                exe.display()
            )));
        }
        return Err(error).io_context(|| format!("failed to write {}", staged.display()));
    }
    let installed = fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
        .and_then(|()| fs::rename(&staged, exe));
    if installed.is_err() {
        let _ = fs::remove_file(&staged);
    }
    installed.io_context(|| format!("failed to replace {}", exe.display()))
}

fn installed_by_cargo(exe: &Path) -> bool {
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    let Some(bin) = cargo_home.map(|home| home.join("bin")) else {
        return false;
    };
    let bin = fs::canonicalize(&bin).unwrap_or(bin);
    exe.parent() == Some(bin.as_path())
}

fn cargo_install(version: &str) -> Result {
    println!("cargo install forkspace --version {version} --locked");
    run(Command::new("cargo")
        .args(["install", "forkspace", "--locked", "--version", version])
        .stdin(Stdio::null()))
}

fn run(command: &mut Command) -> Result {
    let program = command.get_program().to_string_lossy().into_owned();
    let status = command.status().map_err(|error| {
        ForkspaceError::ProcessFailed(format!("failed to run {program}: {error}"))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(ForkspaceError::ProcessFailed(format!(
            "{program} failed ({status})"
        )))
    }
}

fn which(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn curl_missing(error: &std::io::Error) -> ForkspaceError {
    ForkspaceError::ProcessFailed(format!("upgrading needs curl on PATH ({error})"))
}

/// `major.minor.patch`, with an optional `-pre` suffix that sorts before the release.
fn parse_version(version: &str) -> Option<(u64, u64, u64, bool)> {
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, !pre.is_empty()),
        None => (version, false),
    };
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??, !pre);
    parts.next().is_none().then_some(version)
}

fn newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_numerically_with_prereleases_first() {
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "1.0.0-rc.1"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.0.1"), None);
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn latest_tag_comes_from_the_redirect_target() {
        let url = "https://github.com/berezovyy/forkspace/releases/tag/v0.3.1";
        assert_eq!(tag_from_url(url).as_deref(), Some("0.3.1"));
        // With no release published, GitHub redirects to the releases page itself.
        assert_eq!(
            tag_from_url("https://github.com/berezovyy/forkspace/releases"),
            None
        );
    }
}
