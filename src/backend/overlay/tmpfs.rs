//! RAM-backed overlay layers of workspaces made by `--tmpfs` before 0.3, which kept `upper/`
//! and `work/` on an existing tmpfs (`$XDG_RUNTIME_DIR` or `/dev/shm`). New workspaces are
//! never made that way; such a workspace can still be dropped.

use std::fs;
use std::path::Path;

use crate::error::Result;
use crate::util::IoContext;

/// Remove the tmpfs directory holding `upper`. A directory already gone (after a reboot)
/// is not an error.
pub(crate) fn remove_layers(upper: &Path) -> Result {
    let Some(root) = upper.parent() else {
        return Ok(());
    };
    match fs::remove_dir_all(root) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).io_context(|| format!("remove tmpfs layers {}", root.display()))
        }
        _ => Ok(()),
    }
}
