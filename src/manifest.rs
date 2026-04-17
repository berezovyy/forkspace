use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::backend::ChangeCount;
use crate::error::{ForkspaceError, Result};
use crate::filter::ChangeFilter;
use crate::state::State;
use crate::util::{forkspace_home, WalkDirExt};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ManifestStatus {
    #[default]
    Absent,
    Building,
    Ready,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRow {
    pub path: String,
    pub kind: ManifestKind,
    pub size: u64,
    pub mtime_ns: i128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dirty: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub fn manifest_path(name: &str) -> PathBuf {
    forkspace_home()
        .join("manifests")
        .join(format!("{name}.jsonl"))
}

pub fn encode_path(path: &Path) -> String {
    let mut encoded = String::with_capacity(path.as_os_str().as_bytes().len());
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn decode_path(encoded: &str) -> Result<PathBuf> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(ForkspaceError::StateCorrupted(format!(
                    "invalid manifest path escape in '{encoded}'"
                )));
            }
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            let value = high
                .zip(low)
                .map(|(high, low)| (high << 4) | low)
                .ok_or_else(|| {
                    ForkspaceError::StateCorrupted(format!(
                        "invalid manifest path escape in '{encoded}'"
                    ))
                })?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn timestamp_ns(seconds: i64, nanos: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
}

fn row(root: &Path, entry: &walkdir::DirEntry, fork_time_ns: u128) -> Result<ManifestRow> {
    let path = entry.path();
    let metadata = fs::symlink_metadata(path).map_err(|source| ForkspaceError::Io {
        context: format!("read manifest metadata for {}", path.display()),
        source,
    })?;
    let relative = path.strip_prefix(root).map_err(|_| {
        ForkspaceError::StateFailed(format!(
            "manifest entry {} escaped workspace {}",
            path.display(),
            root.display()
        ))
    })?;
    let kind = if metadata.file_type().is_symlink() {
        ManifestKind::Symlink
    } else if metadata.is_dir() {
        ManifestKind::Dir
    } else {
        ManifestKind::File
    };
    let target = if kind == ManifestKind::Symlink {
        Some(encode_path(&fs::read_link(path).map_err(|source| {
            ForkspaceError::Io {
                context: format!("read symlink {}", path.display()),
                source,
            }
        })?))
    } else {
        None
    };
    let ctime_ns = timestamp_ns(metadata.ctime(), metadata.ctime_nsec());
    Ok(ManifestRow {
        path: encode_path(relative),
        kind,
        size: metadata.len(),
        mtime_ns: timestamp_ns(metadata.mtime(), metadata.mtime_nsec()),
        target,
        dirty: ctime_ns > i128::try_from(fork_time_ns).unwrap_or(i128::MAX),
    })
}

pub fn build(name: &str, view: &Path, fork_time_ns: u128) -> Result<usize> {
    let path = manifest_path(name);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).map_err(|source| ForkspaceError::Io {
        context: format!("create manifest directory {}", directory.display()),
        source,
    })?;
    let temporary = directory.join(format!(".{name}.jsonl.tmp-{}", std::process::id()));
    let file = fs::File::create(&temporary).map_err(|source| ForkspaceError::Io {
        context: format!("create manifest {}", temporary.display()),
        source,
    })?;
    let mut writer = BufWriter::new(file);
    let mut count = 0;
    for entry in WalkDir::new(view).min_depth(1).follow_links(false) {
        let entry = entry.walk_context(view)?;
        let row = row(view, &entry, fork_time_ns)?;
        serde_json::to_writer(&mut writer, &row)
            .map_err(|error| ForkspaceError::Serialization(error.to_string()))?;
        writer
            .write_all(b"\n")
            .map_err(|source| ForkspaceError::Io {
                context: format!("write manifest {}", temporary.display()),
                source,
            })?;
        count += 1;
    }
    let file = writer.into_inner().map_err(|error| ForkspaceError::Io {
        context: format!("flush manifest {}", temporary.display()),
        source: error.into_error(),
    })?;
    file.sync_all().map_err(|source| ForkspaceError::Io {
        context: format!("sync manifest {}", temporary.display()),
        source,
    })?;
    fs::rename(&temporary, &path).map_err(|source| ForkspaceError::Io {
        context: format!("install manifest {}", path.display()),
        source,
    })?;
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|source| ForkspaceError::Io {
            context: format!("sync manifest directory {}", directory.display()),
            source,
        })?;
    Ok(count)
}

pub struct ManifestReader {
    path: PathBuf,
    lines: std::iter::Enumerate<std::io::Lines<BufReader<fs::File>>>,
}

impl Iterator for ManifestReader {
    type Item = Result<ManifestRow>;

    fn next(&mut self) -> Option<Self::Item> {
        let (index, line) = self.lines.next()?;
        Some(
            line.map_err(|source| ForkspaceError::Io {
                context: format!("read manifest {} line {}", self.path.display(), index + 1),
                source,
            })
            .and_then(|line| {
                serde_json::from_str(&line).map_err(|error| {
                    ForkspaceError::StateCorrupted(format!(
                        "invalid manifest {} line {}: {error}",
                        self.path.display(),
                        index + 1
                    ))
                })
            }),
        )
    }
}

pub fn stream(name: &str) -> Result<ManifestReader> {
    let path = manifest_path(name);
    let file = fs::File::open(&path).map_err(|source| ForkspaceError::Io {
        context: format!("open manifest {}", path.display()),
        source,
    })?;
    Ok(ManifestReader {
        path,
        lines: BufReader::new(file).lines().enumerate(),
    })
}

pub fn read(name: &str) -> Result<Vec<ManifestRow>> {
    stream(name)?.collect()
}

fn current_rows(workspace: &Workspace) -> Result<BTreeMap<PathBuf, ManifestRow>> {
    let mut rows = BTreeMap::new();
    for entry in WalkDir::new(&workspace.view_path)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry.walk_context(&workspace.view_path)?;
        let value = row(&workspace.view_path, &entry, workspace.fork_time_ns)?;
        rows.insert(decode_path(&value.path)?, value);
    }
    Ok(rows)
}

fn has_post_fork_writes(workspace: &Workspace) -> Result<bool> {
    for entry in WalkDir::new(&workspace.view_path).follow_links(false) {
        let entry = entry.walk_context(&workspace.view_path)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|source| ForkspaceError::Io {
            context: format!("inspect {} for manifest recovery", entry.path().display()),
            source,
        })?;
        if timestamp_ns(metadata.ctime(), metadata.ctime_nsec())
            > i128::try_from(workspace.fork_time_ns).unwrap_or(i128::MAX)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn visible(filter: Option<&ChangeFilter>, path: &Path) -> bool {
    filter.is_none_or(|filter| filter.is_visible(path, false))
}

pub fn change_count(workspace: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
    match workspace.manifest_status {
        ManifestStatus::Absent | ManifestStatus::Building => return Ok(ChangeCount::Pending),
        ManifestStatus::Degraded => return Ok(ChangeCount::Degraded),
        ManifestStatus::Ready => {}
    }

    let baseline = match read(&workspace.name) {
        Ok(rows) => rows,
        Err(_) if has_post_fork_writes(workspace)? => {
            let _ = mark(&workspace.name, ManifestStatus::Degraded);
            return Ok(ChangeCount::Degraded);
        }
        Err(_) => {
            let _ = mark(&workspace.name, ManifestStatus::Building);
            let _ = remove(&workspace.name);
            let _ = spawn(
                workspace.name.clone(),
                workspace.view_path.clone(),
                workspace.fork_time_ns,
            );
            return Ok(ChangeCount::Pending);
        }
    };
    let mut baseline_map = BTreeMap::new();
    for row in baseline {
        baseline_map.insert(decode_path(&row.path)?, row);
    }
    let current = current_rows(workspace)?;
    let filter = (!include_ignored).then(|| ChangeFilter::build(&workspace.base));
    let mut changed = BTreeSet::new();

    for (path, current_row) in &current {
        if current_row.kind == ManifestKind::Dir || !visible(filter.as_ref(), path) {
            continue;
        }
        match baseline_map.get(path) {
            Some(baseline_row) if baseline_row == current_row && !baseline_row.dirty => {}
            _ => {
                changed.insert(path.clone());
            }
        }
    }
    for (path, baseline_row) in &baseline_map {
        if baseline_row.kind != ManifestKind::Dir
            && !current.contains_key(path)
            && visible(filter.as_ref(), path)
        {
            changed.insert(path.clone());
        }
    }
    Ok(ChangeCount::Ready(changed.len()))
}

pub fn remove(name: &str) -> Result {
    let path = manifest_path(name);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ForkspaceError::Io {
            context: format!("remove manifest {}", path.display()),
            source,
        }),
    }
}

fn mark(name: &str, status: ManifestStatus) -> Result<bool> {
    State::with_state(|state| {
        let Some(workspace) = state.workspaces.get_mut(name) else {
            return Ok(false);
        };
        workspace.manifest_status = status;
        Ok(true)
    })
}

#[cfg(target_os = "macos")]
fn run_background(name: String, view: PathBuf, fork_time_ns: u128) {
    for _ in 0..500 {
        match State::load() {
            Ok(state) if state.workspaces.contains_key(&name) => break,
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => return,
        }
    }
    if !State::load().is_ok_and(|state| state.workspaces.contains_key(&name)) {
        return;
    }
    let status = if build(&name, &view, fork_time_ns).is_ok() {
        ManifestStatus::Ready
    } else {
        ManifestStatus::Degraded
    };
    if !mark(&name, status).unwrap_or(false) {
        let _ = remove(&name);
    }
}

#[cfg(target_os = "macos")]
pub fn spawn(name: String, view: PathBuf, fork_time_ns: u128) -> Result {
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe().map_err(|source| ForkspaceError::Io {
        context: "locate forkspace executable for manifest builder".to_string(),
        source,
    })?;
    let mut command = Command::new(executable);
    command
        .env("FORKSPACE_INTERNAL_MANIFEST_WORKER", "1")
        .env("FORKSPACE_INTERNAL_MANIFEST_NAME", name)
        .env("FORKSPACE_INTERNAL_MANIFEST_VIEW", view)
        .env(
            "FORKSPACE_INTERNAL_MANIFEST_FORK_NS",
            fork_time_ns.to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(|source| ForkspaceError::Io {
        context: "start background manifest builder".to_string(),
        source,
    })?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(_name: String, _view: PathBuf, _fork_time_ns: u128) -> Result {
    Err(ForkspaceError::CapabilityUnsupported {
        diagnosis: "background clone manifests are macOS-only".to_string(),
        remedy: "use the selected Linux backend".to_string(),
    })
}

#[cfg(target_os = "macos")]
pub fn run_worker_from_env() -> Option<Result> {
    std::env::var_os("FORKSPACE_INTERNAL_MANIFEST_WORKER")?;
    let result = (|| {
        let name = std::env::var("FORKSPACE_INTERNAL_MANIFEST_NAME").map_err(|_| {
            ForkspaceError::StateFailed("manifest worker is missing its workspace name".to_string())
        })?;
        let view = std::env::var_os("FORKSPACE_INTERNAL_MANIFEST_VIEW")
            .map(PathBuf::from)
            .ok_or_else(|| {
                ForkspaceError::StateFailed("manifest worker is missing its view path".to_string())
            })?;
        let fork_time_ns = std::env::var("FORKSPACE_INTERNAL_MANIFEST_FORK_NS")
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                ForkspaceError::StateFailed("manifest worker has an invalid fork time".to_string())
            })?;
        run_background(name, view, fork_time_ns);
        Ok(())
    })();
    Some(result)
}

#[cfg(not(target_os = "macos"))]
pub fn run_worker_from_env() -> Option<Result> {
    None
}
