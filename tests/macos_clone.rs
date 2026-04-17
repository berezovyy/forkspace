#[cfg(not(target_os = "macos"))]
#[test]
fn macos_clone_suite_skipped() {
    eprintln!(
        "FORKSPACE_TEST_SKIP {{\"capability\":\"clonefile\",\"reason\":\"macOS APFS test suite\"}}"
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::hash_map::DefaultHasher;
    use std::ffi::CString;
    use std::fs;
    use std::hash::{Hash, Hasher};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    use forkspace::backend::{select_backend, BackendKind};
    use forkspace::cli::DropArgs;
    use forkspace::commands;
    use forkspace::error::ForkspaceError;
    use forkspace::manifest::{self, ManifestStatus};
    use forkspace::state::State;
    use serial_test::serial;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("FORKSPACE_HOME", home.path()) };
        unsafe { std::env::remove_var("FORKSPACE_BACKEND") };
        fs::write(base.path().join("README.md"), "base\n").unwrap();
        (home, base)
    }

    fn fork(base: &std::path::Path, name: &str) -> forkspace::workspace::Workspace {
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env(
                "FORKSPACE_HOME",
                std::env::var_os("FORKSPACE_HOME").unwrap(),
            )
            .args(["fork", base.to_str().unwrap(), "--name", name])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fork failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        State::load().unwrap().workspaces.get(name).unwrap().clone()
    }

    fn wait_for_manifest(name: &str) -> forkspace::workspace::Workspace {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let state = State::load().unwrap();
            let workspace = state.workspaces.get(name).unwrap().clone();
            if workspace.manifest_status == ManifestStatus::Ready {
                return workspace;
            }
            assert_ne!(workspace.manifest_status, ManifestStatus::Degraded);
            assert!(
                Instant::now() < deadline,
                "manifest for {name} did not become ready"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn command_exists(command: &str) -> bool {
        Command::new(command)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn hash_tree(root: &Path) -> u64 {
        fn visit(root: &Path, path: &Path, hasher: &mut DefaultHasher) {
            let mut entries: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                entry.strip_prefix(root).unwrap().hash(hasher);
                let metadata = fs::symlink_metadata(&entry).unwrap();
                metadata.len().hash(hasher);
                if metadata.is_dir() {
                    visit(root, &entry, hasher);
                } else if metadata.is_file() {
                    fs::read(&entry).unwrap().hash(hasher);
                } else if metadata.file_type().is_symlink() {
                    fs::read_link(&entry).unwrap().hash(hasher);
                }
            }
        }
        let mut hasher = DefaultHasher::new();
        visit(root, root, &mut hasher);
        hasher.finish()
    }

    fn available_bytes(path: &Path) -> u64 {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        assert_eq!(unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) }, 0);
        let info = unsafe { info.assume_init() };
        info.f_bavail * u64::from(info.f_bsize)
    }

    fn assert_preflight_golden(name: &str, error: &ForkspaceError) {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/macos-clone")
            .join(format!("{name}.stderr"));
        assert_eq!(format!("{error}\n"), fs::read_to_string(fixture).unwrap());
        assert_eq!(
            error.exit_code().to_string(),
            fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/macos-clone")
                    .join(format!("{name}.code"))
            )
            .unwrap()
            .trim()
        );
    }

    #[test]
    #[serial]
    fn selection_preflight_and_environment_are_macos_native() {
        let (_home, base) = setup();
        let backend = select_backend(None).unwrap();
        assert_eq!(backend.kind(), BackendKind::MacClone);
        backend.preflight(base.path()).unwrap();

        let workspace = fork(base.path(), "native");
        let env = backend.shell_env(&workspace);
        assert!(env.iter().any(|(key, _)| key == "FORKSPACE_VIEW"));
        assert!(!env.iter().any(|(key, _)| key == "FORKSPACE_UPPER"));

        unsafe { std::env::set_var("FORKSPACE_BACKEND", "linux") };
        let error = select_backend(None).err().unwrap();
        assert_eq!(error.exit_code(), 7);
        unsafe { std::env::remove_var("FORKSPACE_BACKEND") };
    }

    #[test]
    #[serial]
    fn fork_is_native_isolated_and_preserves_inherited_dependencies() {
        let (_home, base) = setup();
        fs::create_dir_all(base.path().join("node_modules/react")).unwrap();
        fs::write(
            base.path().join("node_modules/react/index.js"),
            "module.exports = 42;\n",
        )
        .unwrap();
        for index in 0..100_000 {
            let directory = base.path().join(format!("tree/{:03}", index / 1000));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(format!("{index:06}")), b"x").unwrap();
        }
        let base_hash = hash_tree(base.path());
        let free_before = available_bytes(base.path());
        let started = Instant::now();
        let first = fork(base.path(), "first");
        let elapsed = started.elapsed();
        let clone_cost = free_before.saturating_sub(available_bytes(base.path()));
        eprintln!(
            "clone_100k_elapsed_ms={} clone_allocated_bytes={clone_cost}",
            elapsed.as_millis()
        );
        assert!(elapsed < Duration::from_secs(10));
        assert!(clone_cost < 512 * 1024 * 1024);
        assert_eq!(hash_tree(base.path()), base_hash);
        let sibling = fork(base.path(), "sibling");
        assert_eq!(
            fs::read_to_string(first.view_path.join("README.md")).unwrap(),
            "base\n"
        );

        fs::write(first.view_path.join("README.md"), "workspace\n").unwrap();
        assert_eq!(
            fs::read_to_string(base.path().join("README.md")).unwrap(),
            "base\n"
        );
        assert_eq!(
            fs::read_to_string(sibling.view_path.join("README.md")).unwrap(),
            "base\n"
        );

        if Command::new("node").arg("--version").output().is_ok() {
            let status = Command::new("node")
                .args(["-e", "require('react')"])
                .current_dir(&first.view_path)
                .status()
                .unwrap();
            assert!(status.success());
        } else {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"node\",\"reason\":\"node executable unavailable\"}}"
            );
        }
    }

    #[test]
    #[serial]
    fn npm_install_and_npm_ci_are_isolated_in_real_clone_directories() {
        if !command_exists("npm") || !command_exists("node") {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"npm\",\"reason\":\"node and npm are required\"}}"
            );
            return;
        }
        let (_home, base) = setup();
        let package = base.path().join("fixtures/zod");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"zod","version":"1.0.0","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(package.join("index.js"), "module.exports = 'local-zod';\n").unwrap();
        fs::write(
            base.path().join("package.json"),
            r#"{"name":"base","version":"1.0.0"}"#,
        )
        .unwrap();

        let first = fork(base.path(), "npm-first");
        let sibling = fork(base.path(), "npm-sibling");
        let status = Command::new("npm")
            .args(["install", "--no-save", "./fixtures/zod"])
            .current_dir(&first.view_path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(first.view_path.join("node_modules/zod").exists());
        assert!(!base.path().join("node_modules/zod").exists());
        assert!(!sibling.view_path.join("node_modules/zod").exists());

        let status = Command::new("npm")
            .args(["install", "--package-lock-only", "--save", "./fixtures/zod"])
            .current_dir(base.path())
            .status()
            .unwrap();
        assert!(status.success());
        let ci = fork(base.path(), "npm-ci");
        fs::create_dir_all(ci.view_path.join("node_modules/junk")).unwrap();
        let status = Command::new("npm")
            .arg("ci")
            .current_dir(&ci.view_path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(ci.view_path.join("node_modules/zod").exists());
        assert!(!ci.view_path.join("node_modules/junk").exists());
        assert!(!base.path().join("node_modules/zod").exists());
    }

    #[test]
    #[serial]
    fn git_status_and_fsck_work_in_clone() {
        if !command_exists("git") {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"git\",\"reason\":\"git executable unavailable\"}}"
            );
            return;
        }
        let (_home, base) = setup();
        for args in [
            &["init"][..],
            &["config", "user.email", "forkspace@example.invalid"],
            &["config", "user.name", "Forkspace Test"],
            &["add", "."],
            &["commit", "-m", "base"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(base.path())
                .status()
                .unwrap()
                .success());
        }
        let workspace = fork(base.path(), "git");
        let first = Instant::now();
        assert!(Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&workspace.view_path)
            .status()
            .unwrap()
            .success());
        let first = first.elapsed();
        let second = Instant::now();
        assert!(Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&workspace.view_path)
            .status()
            .unwrap()
            .success());
        let second = second.elapsed();
        assert!(Command::new("git")
            .args(["fsck", "--full"])
            .current_dir(&workspace.view_path)
            .status()
            .unwrap()
            .success());
        assert!(second <= first.saturating_mul(5) + Duration::from_millis(250));
    }

    #[test]
    #[serial]
    fn vscode_and_native_file_watcher_smoke_test() {
        if !command_exists("code") || !command_exists("fswatch") {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"vscode-fsevents\",\"reason\":\"code and fswatch executables are required for the interactive smoke test\"}}"
            );
            return;
        }
        let (_home, base) = setup();
        let workspace = fork(base.path(), "vscode");
        let watched = workspace.view_path.join("README.md");
        let mut watcher = Command::new("fswatch")
            .args(["-1", watched.to_str().unwrap()])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        fs::write(&watched, "saved from editor-compatible path\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while watcher.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            watcher.try_wait().unwrap().is_some(),
            "native file watcher did not fire"
        );
        assert!(Command::new("code")
            .args(["--new-window", workspace.view_path.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("code")
            .arg("--status")
            .status()
            .unwrap()
            .success());
    }

    #[test]
    #[serial]
    fn tmpfs_refuses_and_drop_allows_immediate_refork() {
        let (home, base) = setup();
        let error = commands::fork::create_workspace(
            base.path().to_path_buf(),
            "ram",
            None,
            None,
            true,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ForkspaceError::CapabilityUnsupported { .. }
        ));

        let workspace = fork(base.path(), "again");
        let old_view = workspace.view_path.clone();
        let started = Instant::now();
        commands::drop::exec(DropArgs {
            name: Some("again".into()),
            force: false,
            all: false,
            glob: false,
        })
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!old_view.exists());
        assert!(!State::load().unwrap().workspaces.contains_key("again"));
        assert!(fork(base.path(), "again").view_path.exists());
        assert!(home.path().join("trash").exists());
    }

    #[test]
    #[serial]
    fn names_concurrent_fork_and_ephemeral_lifecycle_match_linux_contract() {
        let (_home, base) = setup();
        for name in ["", ".", "..", "a/b", "a\0b"] {
            assert!(matches!(
                commands::fork::create_workspace(
                    base.path().to_path_buf(),
                    name,
                    None,
                    None,
                    false,
                    None,
                ),
                Err(ForkspaceError::InvalidArgument(_))
            ));
        }
        let long = "x".repeat(256);
        assert!(matches!(
            commands::fork::create_workspace(
                base.path().to_path_buf(),
                &long,
                None,
                None,
                false,
                None,
            ),
            Err(ForkspaceError::InvalidArgument(_))
        ));

        let base_path = base.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let base_path = base_path.clone();
                thread::spawn(move || {
                    barrier.wait();
                    Command::new(env!("CARGO_BIN_EXE_forkspace"))
                        .env(
                            "FORKSPACE_HOME",
                            std::env::var_os("FORKSPACE_HOME").unwrap(),
                        )
                        .args(["fork", base_path.to_str().unwrap(), "--name", "winner"])
                        .output()
                        .unwrap()
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| result.status.success())
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.status.code() == Some(4))
                .count(),
            1
        );

        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env(
                "FORKSPACE_HOME",
                std::env::var_os("FORKSPACE_HOME").unwrap(),
            )
            .args([
                "enter",
                "ephemeral-mac",
                "--ephemeral",
                "--from",
                base.path().to_str().unwrap(),
                "--",
                "true",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!State::load()
            .unwrap()
            .workspaces
            .contains_key("ephemeral-mac"));
    }

    #[test]
    #[serial]
    fn forkspaceclean_remains_linux_overlay_specific() {
        let (_home, base) = setup();
        fs::write(base.path().join(".forkspaceclean"), "cache.bin\n").unwrap();
        fs::write(base.path().join("cache.bin"), "native-compatible\n").unwrap();
        let workspace = fork(base.path(), "clean-contract");
        assert!(workspace.view_path.join("cache.bin").exists());
    }

    #[test]
    #[serial]
    fn background_manifest_drives_nonblocking_status_and_recovers_safely() {
        let (home, base) = setup();
        fs::write(base.path().join(".gitignore"), "ignored\n").unwrap();
        for index in 0..2_000 {
            fs::write(base.path().join(format!("file-{index:04}")), b"base").unwrap();
        }
        let workspace = fork(base.path(), "manifest");
        assert!(matches!(
            workspace.manifest_status,
            ManifestStatus::Building | ManifestStatus::Ready
        ));
        let workspace = wait_for_manifest("manifest");
        let rows = manifest::read("manifest").unwrap();
        assert_eq!(rows.len(), 2_002);
        let readme = rows.iter().find(|row| row.path == "README.md").unwrap();
        let metadata = fs::metadata(workspace.view_path.join("README.md")).unwrap();
        assert_eq!(readme.size, 5);
        assert_eq!(
            readme.mtime_ns,
            i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
        );

        let _rebuild = fork(base.path(), "rebuild");
        wait_for_manifest("rebuild");
        fs::write(manifest::manifest_path("rebuild"), b"truncated\n").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .arg("ls")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("  -"));
        wait_for_manifest("rebuild");
        assert_eq!(manifest::read("rebuild").unwrap().len(), 2_002);
        commands::drop::exec(DropArgs {
            name: Some("rebuild".into()),
            force: false,
            all: false,
            glob: false,
        })
        .unwrap();

        State::with_state(|state| {
            state
                .workspaces
                .get_mut("manifest")
                .unwrap()
                .manifest_status = ManifestStatus::Building;
            Ok(())
        })
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["ls"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("  -"));
        State::with_state(|state| {
            state
                .workspaces
                .get_mut("manifest")
                .unwrap()
                .manifest_status = ManifestStatus::Ready;
            Ok(())
        })
        .unwrap();

        fs::write(workspace.view_path.join("file-0000"), b"changed").unwrap();
        fs::remove_file(workspace.view_path.join("file-0001")).unwrap();
        fs::write(workspace.view_path.join("new-file"), b"new").unwrap();
        fs::write(workspace.view_path.join("ignored"), b"ignored").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["ls"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("3 files"));
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["ls", "--no-gitignore"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("4 files"));

        fs::write(manifest::manifest_path("manifest"), b"truncated\n").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .arg("ls")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("degraded"));
        assert_eq!(
            State::load()
                .unwrap()
                .workspaces
                .get("manifest")
                .unwrap()
                .manifest_status,
            ManifestStatus::Degraded
        );

        commands::drop::exec(DropArgs {
            name: Some("manifest".into()),
            force: false,
            all: false,
            glob: false,
        })
        .unwrap();
        assert!(!manifest::manifest_path("manifest").exists());
    }

    #[test]
    #[serial]
    fn fork_manifest_round_trips_byte_paths_long_paths_and_nfd() {
        let (_home, base) = setup();
        let non_utf = std::ffi::OsString::from_vec(vec![b'n', b'o', b'n', b'-', 0xff]);
        fs::write(base.path().join(&non_utf), b"bytes").unwrap();
        let nfd = "cafe\u{301}";
        fs::write(base.path().join(nfd), b"nfd").unwrap();
        let mut long = std::path::PathBuf::new();
        for index in 0..6 {
            long.push(format!("{index}-{}", "x".repeat(150)));
        }
        fs::create_dir_all(base.path().join(&long)).unwrap();
        long.push("leaf");
        fs::write(base.path().join(&long), b"long").unwrap();

        fork(base.path(), "byte-paths");
        wait_for_manifest("byte-paths");
        let paths: Vec<_> = manifest::stream("byte-paths")
            .unwrap()
            .map(|row| manifest::decode_path(&row.unwrap().path).unwrap())
            .collect();

        assert!(paths
            .iter()
            .any(|path| path.as_os_str().as_bytes() == non_utf.as_os_str().as_bytes()));
        assert!(paths.iter().any(|path| path == Path::new(nfd)));
        assert!(paths.iter().any(|path| path == &long));
    }

    #[test]
    #[serial]
    fn diff_reports_pending_deletions_and_base_advanced_full_content() {
        let (home, base) = setup();
        fs::write(base.path().join("modified"), b"old\n").unwrap();
        fs::write(base.path().join("deleted"), b"old\n").unwrap();
        let workspace = fork(base.path(), "diff-state");
        wait_for_manifest("diff-state");
        thread::sleep(Duration::from_millis(2));
        fs::write(workspace.view_path.join("modified"), b"workspace\n").unwrap();
        fs::write(workspace.view_path.join("added"), b"added\n").unwrap();
        fs::remove_file(workspace.view_path.join("deleted")).unwrap();
        fs::write(base.path().join("modified"), b"base advanced\n").unwrap();

        State::with_state(|state| {
            state
                .workspaces
                .get_mut("diff-state")
                .unwrap()
                .manifest_status = ManifestStatus::Building;
            Ok(())
        })
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["diff", "diff-state", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("pending manifest"));
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let paths: Vec<_> = json["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| change["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"modified"));
        assert!(paths.contains(&"added"));
        assert!(!paths.contains(&"deleted"));

        State::with_state(|state| {
            state
                .workspaces
                .get_mut("diff-state")
                .unwrap()
                .manifest_status = ManifestStatus::Ready;
            Ok(())
        })
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["diff", "diff-state", "--full"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("base advanced"));
    }

    #[test]
    #[serial]
    fn git_finder_and_ignored_build_output_leave_default_diff_clean() {
        let (home, base) = setup();
        fs::write(base.path().join(".gitignore"), "target/\n").unwrap();
        let git = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(base.path())
            .status();
        if !git.is_ok_and(|status| status.success()) {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"git\",\"reason\":\"git is unavailable\"}}"
            );
            return;
        }
        let workspace = fork(base.path(), "noise");
        wait_for_manifest("noise");
        let _ = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&workspace.view_path)
            .output()
            .unwrap();
        fs::write(workspace.view_path.join(".DS_Store"), b"finder").unwrap();
        fs::create_dir_all(workspace.view_path.join("target/debug")).unwrap();
        fs::write(workspace.view_path.join("target/debug/build"), b"artifact").unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["diff", "noise", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(json["changes"].as_array().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn warm_clone_diff_scan_of_200k_files_is_subsecond() {
        let (_home, base) = setup();
        for index in 0..200_000 {
            let directory = base.path().join(format!("scan/{:03}", index / 1_000));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(format!("{index:06}")), b"x").unwrap();
        }
        fork(base.path(), "scan-budget");
        let workspace = wait_for_manifest("scan-budget");
        let backend = select_backend(Some(workspace.backend_kind)).unwrap();
        backend.changes(&workspace, false, false).unwrap();
        let started = Instant::now();
        let changes = backend.changes(&workspace, false, false).unwrap();
        assert!(changes.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "warm 200k-file scan took {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[serial]
    fn pending_partial_clone_is_reconciled_and_never_listed() {
        let (home, _base) = setup();
        let partial = home.path().join("workspaces/partial");
        fs::create_dir_all(&partial).unwrap();
        fs::write(partial.join("incomplete"), b"partial").unwrap();
        let state = serde_json::json!({
            "version": 2,
            "workspaces": {},
            "proxy": null,
            "pending_creates": [{
                "name": "partial",
                "path": partial,
                "backend_kind": "mac-clone",
                "pid": 0
            }]
        });
        fs::write(
            home.path().join("state.json"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let reconciled = State::load().unwrap();
        assert!(!reconciled.workspaces.contains_key("partial"));
        assert!(!home.path().join("workspaces/partial").exists());
    }

    #[test]
    #[serial]
    fn killing_cli_after_intent_reconciles_partial_clone() {
        let (home, base) = setup();
        for index in 0..100_000 {
            let directory = base.path().join(format!("kill/{:03}", index / 1000));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(format!("{index:06}")), b"x").unwrap();
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args([
                "fork",
                base.path().to_str().unwrap(),
                "--name",
                "interrupted",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let state_path = home.path().join("state.json");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_intent = false;
        while Instant::now() < deadline {
            if let Ok(bytes) = fs::read(&state_path) {
                if serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|state| state["pending_creates"].as_array().cloned())
                    .is_some_and(|pending| !pending.is_empty())
                {
                    saw_intent = true;
                    break;
                }
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        if !saw_intent || child.kill().is_err() {
            let _ = child.wait();
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"kill-mid-clone\",\"reason\":\"clone completed before the signal window\"}}"
            );
            return;
        }
        let _ = child.wait();
        let state = State::load().unwrap();
        assert!(!state.workspaces.contains_key("interrupted"));
        assert!(!home.path().join("workspaces/interrupted").exists());
    }

    #[test]
    #[serial]
    fn nested_workspace_root_error_is_one_line_and_macos_specific() {
        let (_home, base) = setup();
        unsafe { std::env::set_var("FORKSPACE_HOME", base.path().join(".forkspace")) };
        let backend = select_backend(None).unwrap();
        let error = backend.preflight(base.path()).unwrap_err();
        assert_preflight_golden("nested-root", &error);
        let message = error.to_string();
        assert!(!message.contains('\n'));
        assert!(message.contains("FORKSPACE_HOME"));
        assert!(!message.contains("FUSE"));
        assert!(!message.contains("brew"));
    }

    #[test]
    #[serial]
    fn cross_volume_and_non_apfs_preflight_diagnostics_when_locations_are_available() {
        let (_home, base) = setup();
        if let Some(root) = std::env::var_os("FORKSPACE_TEST_CROSS_VOLUME_HOME") {
            unsafe { std::env::set_var("FORKSPACE_HOME", root) };
            let error = select_backend(None)
                .unwrap()
                .preflight(base.path())
                .unwrap_err();
            assert_preflight_golden("cross-volume", &error);
            assert_eq!(error.exit_code(), 6);
            let message = error.to_string();
            assert!(message.contains("different volumes"));
            assert!(!message.contains('\n'));
            assert!(!message.contains("FUSE"));
            assert!(!message.contains("brew"));
        } else {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"cross-volume\",\"reason\":\"set FORKSPACE_TEST_CROSS_VOLUME_HOME to a writable directory on another volume\"}}"
            );
        }

        let non_apfs_base = std::env::var_os("FORKSPACE_TEST_NON_APFS_BASE");
        let non_apfs_home = std::env::var_os("FORKSPACE_TEST_NON_APFS_HOME");
        if let (Some(base), Some(home)) = (non_apfs_base, non_apfs_home) {
            unsafe { std::env::set_var("FORKSPACE_HOME", home) };
            let error = select_backend(None)
                .unwrap()
                .preflight(Path::new(&base))
                .unwrap_err();
            assert_preflight_golden("non-apfs", &error);
            assert_eq!(error.exit_code(), 6);
            let message = error.to_string();
            assert!(message.contains("not on an APFS"));
            assert!(!message.contains('\n'));
            assert!(!message.contains("FUSE"));
            assert!(!message.contains("brew"));
        } else {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"non-apfs\",\"reason\":\"set FORKSPACE_TEST_NON_APFS_BASE and FORKSPACE_TEST_NON_APFS_HOME\"}}"
            );
        }
    }
}
