//! One jobserver shared by every workspace, so parallel builds take turns instead of each
//! starting a job per CPU: five agents running `cargo build` at once on 16 CPUs would
//! otherwise run up to 80 compilers, and run the machine out of memory long before that.
//!
//! It is a GNU make "fifo" jobserver (`--jobserver-auth=fifo:PATH`, make 4.4+, which cargo
//! and the `jobserver` crate understand): a named pipe in
//! `$TMPDIR/forkspace-<uid>/jobserver/` holding one byte per job slot. A build takes a byte
//! before starting a job and puts it back after. Commands started by `enter` get it as
//! `CARGO_MAKEFLAGS`, measured to hold `cargo build` to 3 compilers at a time with 2 slots,
//! where it otherwise ran 16 (2026-11-19). `MAKEFLAGS` is left alone: a `-j` there would make
//! plain `make` run in parallel.
//!
//! A pipe drops its bytes when the last process holding it open closes it, so every
//! `enter` holds it open while its command runs. The first holder, the one that takes the
//! lock beside the pipe exclusively, puts the slots in; the others share the lock. Slots a
//! killed build never returned are therefore restored once no `enter` holds the pipe.
//! There is one slot per CPU.

use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::paths;

/// The directory holding the pipe and its lock.
pub fn dir() -> PathBuf {
    paths::private_tmp_root().join("jobserver")
}

/// A build already under a jobserver (forkspace run inside `make -j`) keeps it.
fn under_another_jobserver() -> bool {
    std::env::var_os("CARGO_MAKEFLAGS").is_some()
        || std::env::var("MAKEFLAGS").is_ok_and(|flags| flags.contains("jobserver"))
}

/// How many jobs may run at once across all workspaces.
pub fn slots() -> usize {
    std::thread::available_parallelism().map_or(4, |count| count.get())
}

/// This process's hold on the shared jobserver; the pipe keeps its slots while any holder
/// lives.
pub struct Holder {
    fifo: PathBuf,
    _pipe: fs::File,
    _lock: fs::File,
}

impl Holder {
    /// The `CARGO_MAKEFLAGS` value that points builds at the jobserver.
    pub fn makeflags(&self) -> String {
        format!("-j --jobserver-auth=fifo:{}", self.fifo.display())
    }
}

/// Join the shared jobserver, creating it if needed. `None` under another jobserver, or
/// when it cannot be set up (builds then run as they would without forkspace).
pub fn join() -> Option<Holder> {
    if under_another_jobserver() {
        return None;
    }
    let dir = dir();
    if let Err(reason) = crate::workspace::tmp::owned_private_dir(&paths::private_tmp_root())
        .and_then(|()| crate::workspace::tmp::owned_private_dir(&dir))
    {
        eprintln!("warning: no shared jobserver ({reason}); builds pick their own job count");
        return None;
    }
    let fifo = dir.join("fifo");
    ensure_fifo(&fifo).ok()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("lock"))
        .ok()?;
    // Read and write: a holder never waits for a peer to open the other end, and the pipe
    // keeps its bytes while it is open.
    let mut pipe = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .ok()?;
    // SAFETY: flock only reads its arguments.
    let first = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if first {
        // No other holder: the pipe is empty (or holds what a still-running build put back,
        // which is at most a few extra slots for a while). Fill it.
        let _ = pipe.write_all(&vec![b'+'; slots()]);
    }
    // SAFETY: as above. Turns the exclusive lock into a shared one, or waits for the first
    // holder to finish filling the pipe.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH) };
    Some(Holder {
        fifo,
        _pipe: pipe,
        _lock: lock,
    })
}

/// Make `path` a named pipe of this user, or check that it is one.
fn ensure_fifo(path: &Path) -> std::io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        if metadata.file_type().is_fifo() && metadata.uid() == uid {
            return Ok(());
        }
        return Err(std::io::Error::other("not a named pipe of this user"));
    }
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(std::io::Error::other)?;
    // SAFETY: `name` is NUL-terminated.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Ok(())
}
