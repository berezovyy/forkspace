#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

pub fn reflink_supported(base: &Path, destination_parent: &Path) -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (base, destination_parent);
        false
    }
    #[cfg(target_os = "linux")]
    {
        if fs::create_dir_all(destination_parent).is_err() {
            return false;
        }
        let (Ok(base_meta), Ok(destination_meta)) =
            (fs::metadata(base), fs::metadata(destination_parent))
        else {
            return false;
        };
        if base_meta.dev() != destination_meta.dev() {
            return false;
        }
        let token = format!("{}-{}", std::process::id(), crate::util::now_unix_ns());
        let source = destination_parent.join(format!(".forkspace-reflink-source-{token}"));
        let target = destination_parent.join(format!(".forkspace-reflink-target-{token}"));
        let result = (|| {
            fs::write(&source, b"x").ok()?;
            let source = fs::File::open(&source).ok()?;
            let target = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)
                .ok()?;
            (unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0)
                .then_some(())
        })()
        .is_some();
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&target);
        result
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn clone_file(source: &fs::File, target: &fs::File) -> bool {
    (unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) }) == 0
}
