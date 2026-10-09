//! Which processes are using a workspace, found with each OS's own process information, and
//! stopping them.
//!
//! A process belongs to a workspace when its working directory or root is inside the
//! workspace's directories, or (Linux) it runs in the workspace's systemd slice. Such
//! processes are "owned": `drop --kill` may stop them. A process that only has files open or
//! mapped there (an editor, say) blocks a drop too, but is never killed.
//!
//! Linux reads `/proc/<pid>/{cwd,root,fd,maps}`; macOS uses libproc
//! (`proc_listallpids`, `proc_pidinfo`, `proc_pidfdinfo`), which needs no privileges for the
//! user's own processes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::backend::BackendState;
use crate::paths;
use crate::workspace::Workspace;

use super::systemd;

/// A process using a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub pid: u32,
    pub name: String,
    pub command: String,
    /// Runs in the workspace (working directory, root, or slice), so `--kill` may stop it.
    pub owned: bool,
}

impl Member {
    /// `node (pid 4312, next dev)`.
    pub fn describe(&self) -> String {
        if self.command.is_empty() || self.command == self.name {
            format!("{} (pid {})", self.name, self.pid)
        } else {
            format!("{} (pid {}, {})", self.name, self.pid, self.command)
        }
    }
}

/// Every process using `workspace`, including ones that only have files open there.
pub fn find(workspace: &Workspace) -> Vec<Member> {
    let roots = roots(workspace);
    let in_slice: HashSet<u32> = systemd::slice_pids(&workspace.name).into_iter().collect();
    let excluded = own_lineage();
    let mut members = Vec::new();
    for pid in os::all_pids() {
        if excluded.contains(&pid) {
            continue;
        }
        let usage = if in_slice.contains(&pid) {
            Some(true)
        } else {
            os::usage(pid, &roots, true)
        };
        if let Some(owned) = usage {
            let found = member(pid, owned);
            // The fuse-overlayfs daemon serving the view holds its layers open; unmounting
            // stops it.
            if found.name != "fuse-overlayfs" {
                members.push(found);
            }
        }
    }
    members.sort_by_key(|member| member.pid);
    members
}

/// For each of `workspaces`, how many processes run in it (working directory, root, or
/// slice). Cheaper than [`find`]: open files are not inspected.
pub fn counts<'a>(workspaces: impl IntoIterator<Item = &'a Workspace>) -> HashMap<String, usize> {
    let workspaces: Vec<_> = workspaces.into_iter().collect();
    let roots: Vec<(String, Vec<PathBuf>)> = workspaces
        .iter()
        .map(|workspace| (workspace.name.clone(), roots(workspace)))
        .collect();
    let mut counted: HashMap<String, HashSet<u32>> = workspaces
        .iter()
        .map(|workspace| {
            let in_slice = systemd::slice_pids(&workspace.name).into_iter().collect();
            (workspace.name.clone(), in_slice)
        })
        .collect();
    let excluded = own_lineage();
    for pid in os::all_pids() {
        if excluded.contains(&pid) {
            continue;
        }
        for (name, roots) in &roots {
            if os::usage(pid, roots, false).is_some() {
                counted.entry(name.clone()).or_default().insert(pid);
            }
        }
    }
    counted
        .into_iter()
        .map(|(name, pids)| {
            let alive = pids.into_iter().filter(|pid| os::alive(*pid)).count();
            (name, alive)
        })
        .collect()
}

/// Stop the owned processes among `members`: SIGTERM, up to three seconds to exit, then
/// SIGKILL, and finally a kill of the whole slice. Processes that are not owned are left.
pub fn stop(workspace: &Workspace, members: &[Member]) {
    let owned: Vec<u32> = members
        .iter()
        .filter(|member| member.owned)
        .map(|member| member.pid)
        .collect();
    signal(&owned, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(3);
    while owned.iter().any(|pid| os::alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survivors: Vec<u32> = owned.into_iter().filter(|pid| os::alive(*pid)).collect();
    signal(&survivors, libc::SIGKILL);
    systemd::kill_slice(&workspace.name);
    let deadline = Instant::now() + Duration::from_secs(2);
    while survivors.iter().any(|pid| os::alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal(pids: &[u32], signal: libc::c_int) {
    for pid in pids {
        if let Ok(pid) = libc::pid_t::try_from(*pid) {
            unsafe { libc::kill(pid, signal) };
        }
    }
}

fn member(pid: u32, owned: bool) -> Member {
    let (name, command) = os::describe(pid);
    Member {
        pid,
        name,
        command,
        owned,
    }
}

/// The directories that belong to `workspace`, canonicalized where possible.
fn roots(workspace: &Workspace) -> Vec<PathBuf> {
    let mut roots = vec![workspace.view_path.clone()];
    if let BackendState::Overlay(layers) = &workspace.backend_state {
        roots.push(paths::overlay_root(&workspace.name));
        roots.push(layers.upper.clone());
        roots.push(layers.work.clone());
    }
    let mut canonical: Vec<PathBuf> = roots
        .into_iter()
        .map(|root| root.canonicalize().unwrap_or(root))
        .collect();
    canonical.sort();
    canonical.dedup();
    canonical
}

fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// This process and its ancestors: never reported, so `drop` run from a shell inside the
/// workspace does not count or stop that shell.
fn own_lineage() -> HashSet<u32> {
    let mut lineage = HashSet::new();
    let mut pid = std::process::id();
    while pid > 1 && lineage.insert(pid) {
        match os::parent(pid) {
            Some(parent) => pid = parent,
            None => break,
        }
    }
    lineage
}

#[cfg(target_os = "linux")]
mod os {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::inside;

    pub fn all_pids() -> Vec<u32> {
        fs::read_dir("/proc")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `Some(true)` when the working directory or root is inside `roots`, `Some(false)` when
    /// (with `deep`) only an open file or mapping is, `None` otherwise.
    pub fn usage(pid: u32, roots: &[PathBuf], deep: bool) -> Option<bool> {
        let proc = PathBuf::from(format!("/proc/{pid}"));
        for link in ["cwd", "root"] {
            if fs::read_link(proc.join(link)).is_ok_and(|path| inside(&clean(path), roots)) {
                return Some(true);
            }
        }
        if !deep {
            return None;
        }
        if let Ok(entries) = fs::read_dir(proc.join("fd")) {
            for entry in entries.flatten() {
                if fs::read_link(entry.path()).is_ok_and(|path| inside(&clean(path), roots)) {
                    return Some(false);
                }
            }
        }
        let maps = fs::read_to_string(proc.join("maps")).unwrap_or_default();
        maps.lines()
            .filter_map(|line| line.split_whitespace().nth(5))
            .any(|path| inside(Path::new(path), roots))
            .then_some(false)
    }

    /// `/proc` appends ` (deleted)` to links whose target is gone.
    fn clean(path: PathBuf) -> PathBuf {
        match path
            .to_str()
            .and_then(|text| text.strip_suffix(" (deleted)"))
        {
            Some(text) => PathBuf::from(text),
            None => path,
        }
    }

    pub fn describe(pid: u32) -> (String, String) {
        let name = fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let command = fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        (name, shorten(&command))
    }

    /// Running and not a zombie.
    pub fn alive(pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X")
        })
    }

    pub fn parent(pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }

    fn shorten(command: &str) -> String {
        let mut short: String = command.chars().take(60).collect();
        if short.len() < command.len() {
            short.push('…');
        }
        short
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::CStr;
    use std::mem::{size_of, MaybeUninit};
    use std::path::PathBuf;

    use super::inside;

    /// `PROC_PIDFDVNODEPATHINFO` from `sys/proc_info.h` (not in the libc crate).
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: libc::off_t,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    struct VnodeFdInfoWithPath {
        pfi: ProcFileInfo,
        pvip: libc::vnode_info_path,
    }

    pub fn all_pids() -> Vec<u32> {
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as libc::pid_t; count as usize + 64];
        let bytes = (pids.len() * size_of::<libc::pid_t>()) as libc::c_int;
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(filled.max(0) as usize);
        pids.into_iter()
            .filter_map(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .collect()
    }

    pub fn usage(pid: u32, roots: &[PathBuf], deep: bool) -> Option<bool> {
        let pid = libc::c_int::try_from(pid).ok()?;
        let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read == size {
            let info = unsafe { info.assume_init() };
            if inside(&vnode_path(&info.pvi_cdir), roots) {
                return Some(true);
            }
        }
        if !deep {
            return None;
        }
        open_files(pid)
            .iter()
            .any(|path| inside(path, roots))
            .then_some(false)
    }

    fn open_files(pid: libc::c_int) -> Vec<PathBuf> {
        let bytes =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return Vec::new();
        }
        let capacity = bytes as usize / size_of::<libc::proc_fdinfo>() + 16;
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (capacity * size_of::<libc::proc_fdinfo>()) as libc::c_int,
            )
        };
        if filled <= 0 {
            return Vec::new();
        }
        unsafe { fds.set_len(filled as usize / size_of::<libc::proc_fdinfo>()) };
        let mut paths = Vec::new();
        for fd in fds {
            if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
                continue;
            }
            let mut info = MaybeUninit::<VnodeFdInfoWithPath>::zeroed();
            let size = size_of::<VnodeFdInfoWithPath>() as libc::c_int;
            let read = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if read == size {
                let info = unsafe { info.assume_init() };
                paths.push(vnode_path(&info.pvip));
            }
        }
        paths
    }

    fn vnode_path(info: &libc::vnode_info_path) -> PathBuf {
        let bytes: &[libc::c_char] =
            unsafe { std::slice::from_raw_parts(info.vip_path.as_ptr().cast(), 32 * 32) };
        let bytes: Vec<u8> = bytes.iter().map(|byte| *byte as u8).collect();
        let text = CStr::from_bytes_until_nul(&bytes)
            .map(|text| text.to_string_lossy().into_owned())
            .unwrap_or_default();
        PathBuf::from(text)
    }

    pub fn describe(pid: u32) -> (String, String) {
        let Ok(pid) = libc::c_int::try_from(pid) else {
            return (String::new(), String::new());
        };
        let mut name = [0u8; 256];
        let length = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
        let name = String::from_utf8_lossy(&name[..length.max(0) as usize]).into_owned();
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let length =
            unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        let path = String::from_utf8_lossy(&path[..length.max(0) as usize]).into_owned();
        (name, path)
    }

    pub fn alive(pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        unsafe { libc::kill(pid, 0) == 0 }
    }

    pub fn parent(pid: u32) -> Option<u32> {
        let pid = libc::c_int::try_from(pid).ok()?;
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        (read == size).then(|| unsafe { info.assume_init() }.pbi_ppid)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod os {
    use std::path::PathBuf;

    pub fn all_pids() -> Vec<u32> {
        Vec::new()
    }
    pub fn usage(_pid: u32, _roots: &[PathBuf], _deep: bool) -> Option<bool> {
        None
    }
    pub fn describe(_pid: u32) -> (String, String) {
        (String::new(), String::new())
    }
    pub fn alive(_pid: u32) -> bool {
        false
    }
    pub fn parent(_pid: u32) -> Option<u32> {
        None
    }
}
