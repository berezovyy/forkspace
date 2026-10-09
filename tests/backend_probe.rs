mod common;

use std::ffi::CString;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use forkspace::backend::{select_backend, BackendKind};
use forkspace::changes::manifest;
use serial_test::serial;
use walkdir::WalkDir;

#[test]
#[serial]
fn plain_copy_is_a_privilege_free_fidelity_floor() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::create_dir_all(base.path().join("dir")).unwrap();
    fs::write(base.path().join("dir/file"), b"old\n").unwrap();
    fs::write(base.path().join("mode"), b"mode\n").unwrap();
    fs::write(base.path().join("edit"), b"old\n").unwrap();
    let mut permissions = fs::metadata(base.path().join("mode"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o4750);
    fs::set_permissions(base.path().join("mode"), permissions).unwrap();
    std::os::unix::fs::symlink("dir/file", base.path().join("link")).unwrap();
    std::os::unix::fs::symlink(
        base.path().join("dir/file"),
        base.path().join("absolute-link"),
    )
    .unwrap();
    fs::hard_link(base.path().join("dir/file"), base.path().join("hardlink")).unwrap();
    let sparse = fs::File::create(base.path().join("sparse")).unwrap();
    sparse.set_len(16 * 1024 * 1024).unwrap();
    unsafe {
        libc::lseek(sparse.as_raw_fd(), 8 * 1024 * 1024, libc::SEEK_SET);
        libc::write(sparse.as_raw_fd(), b"middle".as_ptr().cast(), 6);
    }
    let non_utf8 = std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec());
    fs::write(base.path().join(&non_utf8), b"bytes\n").unwrap();
    let long = base
        .path()
        .join("a".repeat(225))
        .join("b".repeat(225))
        .join("c".repeat(225))
        .join("d".repeat(225));
    fs::create_dir_all(long.parent().unwrap()).unwrap();
    fs::write(&long, b"long\n").unwrap();
    set_xattr(
        base.path().join("edit").as_path(),
        "user.forkspace-test",
        b"preserved",
    );

    let workspace = forkspace::workspace::create(
        base.path().to_path_buf(),
        "copy-floor",
        forkspace::workspace::CreateOptions {
            force_copy: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(workspace.backend_kind, BackendKind::PlainCopy);
    assert!(workspace.degraded);
    assert!(workspace
        .probe_reason
        .as_deref()
        .unwrap()
        .contains("full copy"));
    assert_eq!(
        fs::read_link(workspace.view_path.join("link")).unwrap(),
        Path::new("dir/file")
    );
    assert_eq!(
        fs::metadata(workspace.view_path.join("mode"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o4750
    );
    assert_eq!(
        fs::read_link(workspace.view_path.join("absolute-link")).unwrap(),
        base.path().join("dir/file")
    );
    assert_eq!(
        fs::read(workspace.view_path.join(&non_utf8)).unwrap(),
        b"bytes\n"
    );
    assert_eq!(
        fs::read(
            workspace
                .view_path
                .join(long.strip_prefix(base.path()).unwrap())
        )
        .unwrap(),
        b"long\n"
    );
    assert_eq!(
        get_xattr(
            workspace.view_path.join("edit").as_path(),
            "user.forkspace-test"
        ),
        b"preserved"
    );
    let source_sparse = fs::metadata(base.path().join("sparse")).unwrap();
    let copied_sparse = fs::metadata(workspace.view_path.join("sparse")).unwrap();
    assert_eq!(copied_sparse.len(), source_sparse.len());
    assert!(copied_sparse.blocks() <= source_sparse.blocks() + 16);
    assert_eq!(
        fs::metadata(workspace.view_path.join("dir/file"))
            .unwrap()
            .ino(),
        fs::metadata(workspace.view_path.join("hardlink"))
            .unwrap()
            .ino()
    );
    let max_ctime = WalkDir::new(&workspace.view_path)
        .into_iter()
        .map(|entry| fs::symlink_metadata(entry.unwrap().path()).unwrap())
        .map(|metadata| {
            metadata.ctime().max(0) as u128 * 1_000_000_000 + metadata.ctime_nsec().max(0) as u128
        })
        .max()
        .unwrap();
    assert!(workspace.fork_time_ns >= max_ctime);
    let backend = select_backend(Some(workspace.backend_kind)).unwrap();
    assert!(backend.changes(&workspace, true, false).unwrap().is_empty());

    fs::write(workspace.view_path.join("edit"), b"changed\n").unwrap();
    let changes = backend.changes(&workspace, true, false).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, Path::new("edit"));
    let plan = forkspace::sync::plan::build(&workspace, false).unwrap();
    let journal = forkspace::sync::merge::execute(&plan).unwrap();
    forkspace::sync::journal::remove(&journal).unwrap();
    assert_eq!(fs::read(base.path().join("edit")).unwrap(), b"changed\n");

    forkspace::commands::drop::exec(forkspace::cli::DropArgs {
        name: Some("copy-floor".to_string()),
        kill: false,
        force: false,
        all: false,
        glob: false,
    })
    .unwrap();
    assert!(!manifest::manifest_path("copy-floor").exists());
}

#[test]
#[serial]
fn reflink_probe_is_ioctl_based_and_override_is_sticky() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"x").unwrap();
    let _honest_runtime_verdict = forkspace::backend::probe::reflink_supported(
        base.path(),
        &forkspace::paths::forkspace_home().join("workspaces"),
    );
    // The reflink probe must try a real FICLONE rather than guess from the filesystem type.
    let source = fs::read_to_string("src/backend/snapshot/reflink.rs").unwrap();
    assert!(source.contains("libc::ioctl"));
    assert!(source.contains("FICLONE"));
    assert!(!source.contains("f_type"));

    let inherited = std::env::var_os("FORKSPACE_BACKEND");
    unsafe { std::env::set_var("FORKSPACE_BACKEND", "copy") };
    assert_eq!(select_backend(None).unwrap().kind(), BackendKind::PlainCopy);
    unsafe {
        if let Some(inherited) = inherited {
            std::env::set_var("FORKSPACE_BACKEND", inherited);
        } else {
            std::env::remove_var("FORKSPACE_BACKEND");
        }
    }
}

#[test]
#[serial]
fn probe_report_is_ordered_and_uses_the_ioctl_verdict() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"x").unwrap();
    #[cfg(target_os = "linux")]
    {
        let destination = forkspace::paths::forkspace_home().join("workspaces");
        let report = forkspace::backend::probe::linux_report(base.path(), &destination);
        assert_eq!(
            report
                .candidates
                .iter()
                .map(|candidate| candidate.backend)
                .collect::<Vec<_>>(),
            [
                BackendKind::Overlay,
                BackendKind::LinuxReflink,
                BackendKind::PlainCopy
            ]
        );
        assert_eq!(
            report.candidates[1].available,
            forkspace::backend::probe::reflink_supported(base.path(), &destination)
        );
        assert!(report.winner().is_some());
        assert!(report.winning_reason().is_some());
    }
}

#[test]
#[serial]
fn requested_real_reflink_volume_materializes_without_data_copy() {
    let Some(root) = std::env::var_os("FORKSPACE_TEST_REFLINK_ROOT") else {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"reflink-volume\",\"reason\":\"set FORKSPACE_TEST_REFLINK_ROOT to a writable Btrfs or XFS-reflink volume\"}}"
        );
        return;
    };
    let root = Path::new(&root);
    let base = root.join(format!("probe-base-{}", std::process::id()));
    let home = root.join(format!("probe-home-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("file"), vec![7_u8; 4 * 1024 * 1024]).unwrap();
    let inherited_backend = std::env::var_os("FORKSPACE_BACKEND");
    unsafe {
        std::env::set_var("FORKSPACE_HOME", &home);
        std::env::set_var("FORKSPACE_BACKEND", "linux-reflink");
    }
    assert!(forkspace::backend::probe::reflink_supported(
        &base,
        &home.join("workspaces")
    ));
    let workspace =
        forkspace::workspace::create(base.clone(), "real-reflink", Default::default()).unwrap();
    assert_eq!(workspace.backend_kind, BackendKind::LinuxReflink);
    let forkspace::backend::BackendState::CowDir {
        reflinked_files,
        copied_files,
        ..
    } = workspace.backend_state
    else {
        panic!("expected directory backend state")
    };
    assert_eq!(reflinked_files, 1);
    assert_eq!(copied_files, 0);
    unsafe {
        if let Some(inherited) = inherited_backend {
            std::env::set_var("FORKSPACE_BACKEND", inherited);
        } else {
            std::env::remove_var("FORKSPACE_BACKEND");
        }
    }
    let _ = fs::remove_dir_all(base);
    let _ = fs::remove_dir_all(home);
}

#[cfg(target_os = "linux")]
#[test]
#[serial]
fn reflink_mixed_submount_falls_back_per_file_without_failing_the_fork() {
    let Some(base) = std::env::var_os("FORKSPACE_TEST_MIXED_BASE") else {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"reflink-submount\",\"reason\":\"set FORKSPACE_TEST_MIXED_BASE to a reflink-volume directory containing a mounted sub directory\"}}"
        );
        return;
    };
    let Some(root) = std::env::var_os("FORKSPACE_TEST_REFLINK_ROOT") else {
        eprintln!(
            "FORKSPACE_TEST_SKIP {{\"capability\":\"reflink-volume\",\"reason\":\"set FORKSPACE_TEST_REFLINK_ROOT with FORKSPACE_TEST_MIXED_BASE\"}}"
        );
        return;
    };
    let base = Path::new(&base);
    fs::write(base.join("outer"), vec![1_u8; 1024 * 1024]).unwrap();
    fs::write(base.join("sub/inner"), vec![2_u8; 1024 * 1024]).unwrap();
    let home = Path::new(&root).join(format!("mixed-home-{}", std::process::id()));
    let inherited_home = std::env::var_os("FORKSPACE_HOME");
    let inherited_backend = std::env::var_os("FORKSPACE_BACKEND");
    unsafe {
        std::env::set_var("FORKSPACE_HOME", &home);
        std::env::set_var("FORKSPACE_BACKEND", "linux-reflink");
    }
    let workspace =
        forkspace::workspace::create(base.to_path_buf(), "mixed", Default::default()).unwrap();
    let forkspace::backend::BackendState::CowDir {
        reflinked_files,
        copied_files,
        ..
    } = workspace.backend_state
    else {
        panic!("expected directory backend state")
    };
    assert_eq!(reflinked_files, 1);
    assert_eq!(copied_files, 1);
    assert_eq!(fs::read(workspace.view_path.join("outer")).unwrap()[0], 1);
    assert_eq!(
        fs::read(workspace.view_path.join("sub/inner")).unwrap()[0],
        2
    );
    unsafe {
        match inherited_home {
            Some(value) => std::env::set_var("FORKSPACE_HOME", value),
            None => std::env::remove_var("FORKSPACE_HOME"),
        }
        match inherited_backend {
            Some(value) => std::env::set_var("FORKSPACE_BACKEND", value),
            None => std::env::remove_var("FORKSPACE_BACKEND"),
        }
    }
    let _ = fs::remove_dir_all(home);
}

fn set_xattr(path: &Path, name: &str, value: &[u8]) {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new(name).unwrap();
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    assert_eq!(result, 0, "setxattr: {}", std::io::Error::last_os_error());
}

fn get_xattr(path: &Path, name: &str) -> Vec<u8> {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new(name).unwrap();
    #[cfg(target_os = "linux")]
    let size = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    #[cfg(target_os = "macos")]
    let size =
        unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
    assert!(size >= 0, "getxattr: {}", std::io::Error::last_os_error());
    let mut value = vec![0_u8; size as usize];
    #[cfg(target_os = "linux")]
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    #[cfg(target_os = "macos")]
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    assert_eq!(read, size);
    value
}
