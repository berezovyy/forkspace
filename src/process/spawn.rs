//! Spawning detached background processes that log to files.

use std::fs::File;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};

/// Spawn `command` as the leader of a new session, so it outlives forkspace and can be
/// signalled as a process group.
pub(super) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    };
    command.spawn()
}

/// Create (truncating) the stdout and stderr log files `dir/stdout_name` and `dir/stderr_name`.
pub(super) fn create_logs(
    dir: &Path,
    stdout_name: &str,
    stderr_name: &str,
) -> io::Result<(File, File)> {
    let stdout = File::create(dir.join(stdout_name))?;
    let stderr = File::create(dir.join(stderr_name))?;
    Ok((stdout, stderr))
}
