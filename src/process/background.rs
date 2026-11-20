//! Background work forkspace starts for itself (trash removal, flushing a frozen copy, the Git
//! index refresh) runs at the lowest CPU and disk priority, so it never competes with the
//! commands agents and the user run.

use std::process::Command;

/// Make `command` run at the lowest priority: nice 19 and the idle I/O class on Linux (it gets
/// disk time only when no one else wants it; both need no privileges for one's own process),
/// the background band on macOS (which throttles its CPU and disk I/O).
pub(crate) fn lower(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs between fork and exec and only makes the setpriority and
    // ioprio_set system calls, which are async-signal-safe. Failure is ignored: the work
    // still runs, at normal priority.
    unsafe {
        command.pre_exec(|| {
            lower_this_process();
            Ok(())
        })
    }
}

#[cfg(target_os = "linux")]
fn lower_this_process() {
    /// `IOPRIO_WHO_PROCESS` and `IOPRIO_CLASS_IDLE` from `linux/ioprio.h`.
    const WHO_PROCESS: libc::c_int = 1;
    const CLASS_IDLE: libc::c_int = 3;
    const CLASS_SHIFT: libc::c_int = 13;
    // SAFETY: both calls only read their arguments.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        libc::syscall(
            libc::SYS_ioprio_set,
            WHO_PROCESS,
            0,
            CLASS_IDLE << CLASS_SHIFT,
        );
    }
}

#[cfg(target_os = "macos")]
fn lower_this_process() {
    // SAFETY: setpriority only reads its arguments.
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn lower_this_process() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn lowered_commands_run_niced_in_the_idle_io_class() {
        let mut command = Command::new("sleep");
        command.arg("2");
        let mut child = lower(&mut command).spawn().unwrap();
        let pid = child.id() as libc::c_int;
        // SAFETY: both calls only read their arguments.
        let (ioprio, nice) = unsafe {
            (
                libc::syscall(libc::SYS_ioprio_get, 1, pid),
                libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t),
            )
        };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(ioprio >> 13, 3, "I/O class");
        assert_eq!(nice, 19);
    }
}
