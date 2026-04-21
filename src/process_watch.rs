#[cfg(target_os = "macos")]
use std::process::Command;

pub fn identity(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ")) {
            if let Some(value) = bytes
                .split(|byte| *byte == 0)
                .find_map(|entry| entry.strip_prefix(b"FORKSPACE_PROCESS_IDENTITY="))
            {
                return Some(String::from_utf8_lossy(value).into_owned());
            }
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)
            .map(str::to_string)
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = Command::new("ps")
            .args(["eww", "-p", &pid.to_string(), "-o", "command="])
            .output()
        {
            if let Some(value) = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .find_map(|field| field.strip_prefix("FORKSPACE_PROCESS_IDENTITY="))
            {
                return Some(value.to_string());
            }
        }
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

pub fn new_identity() -> String {
    format!(
        "forkspace-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

#[cfg(target_os = "macos")]
fn kqueue_alive(pid: u32) -> bool {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return false;
    }
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let registered =
        unsafe { libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } == 0;
    let _ = unsafe { libc::close(queue) };
    registered
}

pub fn is_alive(pid: u32, expected_identity: Option<&str>) -> bool {
    if expected_identity.is_some_and(|expected| identity(pid).as_deref() != Some(expected)) {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        kqueue_alive(pid)
    }
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

pub fn terminate_group(pid: u32, expected_identity: Option<&str>) -> bool {
    if !is_alive(pid, expected_identity) {
        return false;
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    if is_alive(pid, expected_identity) {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    true
}
