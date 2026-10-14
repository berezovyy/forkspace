use std::os::unix::process::CommandExt;
use std::process::Command;

#[test]
fn stale_identity_cannot_kill_a_process_group() {
    let mut command = Command::new("sleep");
    command.arg("30");
    let token = forkspace::process::watch::new_identity();
    command.env("FORKSPACE_PROCESS_IDENTITY", &token);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let pid = child.id();
    let identity = forkspace::process::watch::identity(pid).unwrap();
    assert_eq!(
        forkspace::process::watch::spawned_identity(pid, token.clone()),
        identity
    );
    // Linux reads the token from the environment; macOS hides the environment of platform
    // binaries such as `sleep`, so it uses the start time.
    if cfg!(target_os = "linux") {
        assert_eq!(identity, token);
    }
    assert!(forkspace::process::watch::is_alive(pid, Some(&identity)));
    assert!(!forkspace::process::watch::terminate_group(
        pid,
        Some("recycled-process-identity")
    ));
    assert!(child.try_wait().unwrap().is_none());
    assert!(forkspace::process::watch::terminate_group(
        pid,
        Some(&identity)
    ));
    child.wait().unwrap();
    assert!(!forkspace::process::watch::is_alive(pid, Some(&identity)));
}
