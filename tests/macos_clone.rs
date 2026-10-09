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
    use forkspace::changes::manifest::{self, ManifestStatus};
    use forkspace::cli::DropArgs;
    use forkspace::commands;
    use forkspace::error::ForkspaceError;
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
            .args(["new", name, "--from", base.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fork failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        State::load().unwrap().workspaces.get(name).unwrap().clone()
    }

    /// Settles whether change tracking depends on APFS keeping ctimes across `clonefile`:
    /// a fresh fork must be clean, its manifest rows must not be dirty, and a file deleted
    /// right after `new` returns (before the background manifest walks it) must still be
    /// reported as deleted.
    #[test]
    #[serial]
    fn fresh_clone_is_clean_and_early_deletions_are_tracked() {
        let (_home, base) = setup();
        fs::create_dir_all(base.path().join("src/nested")).unwrap();
        for index in 0..200 {
            fs::write(base.path().join(format!("src/nested/file-{index}")), "x\n").unwrap();
        }
        fs::write(base.path().join("gone-early"), "delete me\n").unwrap();

        let clean = fork(base.path(), "clean-clone");
        let clean = wait_for_manifest(&clean.name);
        let dirty_rows = manifest::read(&clean.name)
            .unwrap()
            .into_iter()
            .filter(|row| row.dirty)
            .map(|row| row.path)
            .collect::<Vec<_>>();
        assert!(
            dirty_rows.is_empty(),
            "rows marked dirty at fork: {dirty_rows:?}"
        );
        let diff = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env(
                "FORKSPACE_HOME",
                std::env::var_os("FORKSPACE_HOME").unwrap(),
            )
            .args(["diff", "clean-clone", "--files"])
            .output()
            .unwrap();
        assert!(diff.status.success());
        let stdout = String::from_utf8_lossy(&diff.stdout);
        assert!(
            stdout.starts_with("clean"),
            "fresh clone reports changes:\n{stdout}"
        );

        let early = fork(base.path(), "early-delete");
        fs::remove_file(early.view_path.join("gone-early")).unwrap();
        wait_for_manifest(&early.name);
        let diff = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env(
                "FORKSPACE_HOME",
                std::env::var_os("FORKSPACE_HOME").unwrap(),
            )
            .args(["diff", "early-delete", "--files"])
            .output()
            .unwrap();
        assert!(diff.status.success());
        assert!(
            String::from_utf8_lossy(&diff.stdout).contains("gone-early"),
            "{}",
            String::from_utf8_lossy(&diff.stdout)
        );
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
        let mut rendered = format!("{error}\n");
        if name == "non-apfs" {
            let prefix = "backend unavailable: ";
            let marker = "; APFS clones are unavailable there";
            if let Some((_, tail)) = rendered
                .strip_prefix(prefix)
                .and_then(|value| value.split_once(marker))
            {
                rendered = format!("{prefix}<BASE> is <FILESYSTEM>{marker}{tail}");
            }
        }
        assert_eq!(rendered, fs::read_to_string(fixture).unwrap());
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
    fn cloned_large_file_consumes_less_than_one_megabyte_of_new_blocks() {
        let (_home, base) = setup();
        fs::write(
            base.path().join("large.bin"),
            vec![0x5a_u8; 64 * 1024 * 1024],
        )
        .unwrap();
        let before = available_bytes(base.path());
        let workspace = fork(base.path(), "zero-copy");
        let allocated = before.saturating_sub(available_bytes(base.path()));
        assert!(
            allocated < 1024 * 1024,
            "APFS clone allocated {allocated} bytes for unchanged file data"
        );
        assert_eq!(
            fs::metadata(workspace.view_path.join("large.bin"))
                .unwrap()
                .len(),
            64 * 1024 * 1024
        );
    }

    #[test]
    #[serial]
    fn apfs_clone_preserves_quarantine_and_custom_extended_attributes() {
        if !command_exists("xattr") {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"xattr\",\"reason\":\"xattr executable unavailable\"}}"
            );
            return;
        }
        let (_home, base) = setup();
        let file = base.path().join("downloaded");
        fs::write(&file, b"payload\n").unwrap();
        let quarantine = "0081;00000000;forkspace-tests;00000000-0000-0000-0000-000000000000";
        for (name, value) in [
            ("com.apple.quarantine", quarantine),
            ("com.forkspace.fixture", "preserved"),
        ] {
            let output = Command::new("xattr")
                .args(["-w", name, value])
                .arg(&file)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let workspace = fork(base.path(), "xattrs");
        for (name, expected) in [
            ("com.apple.quarantine", quarantine),
            ("com.forkspace.fixture", "preserved"),
        ] {
            let output = Command::new("xattr")
                .args(["-p", name])
                .arg(workspace.view_path.join("downloaded"))
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
    }

    #[test]
    #[serial]
    fn normalization_case_non_utf8_and_long_paths_do_not_duplicate_or_disappear() {
        let (_home, base) = setup();
        let nfc = "caf\u{e9}";
        let nfd = "cafe\u{301}";
        fs::write(base.path().join(nfc), b"old\n").unwrap();
        assert!(base.path().join(nfd).exists());
        let non_utf8 = std::ffi::OsString::from_vec(b"raw-\xff".to_vec());
        if let Err(error) = fs::write(base.path().join(&non_utf8), b"old\n") {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"non-utf8-filename\",\"reason\":{}}}",
                serde_json::to_string(&error.to_string()).unwrap()
            );
            return;
        }
        let long = (0..30)
            .map(|index| format!("segment-{index:02}-abcdefghijklmnop"))
            .collect::<std::path::PathBuf>();
        fs::create_dir_all(base.path().join(&long).parent().unwrap()).unwrap();
        fs::write(base.path().join(&long), b"old\n").unwrap();

        let case_probe = base.path().join("CaseProbe");
        fs::write(&case_probe, b"case\n").unwrap();
        let case_insensitive = base.path().join("caseprobe").exists();
        let workspace = fork(base.path(), "names");
        let workspace = wait_for_manifest(&workspace.name);
        fs::write(workspace.view_path.join(nfd), b"new\n").unwrap();
        fs::write(workspace.view_path.join(&non_utf8), b"new\n").unwrap();
        fs::write(workspace.view_path.join(&long), b"new\n").unwrap();
        if case_insensitive {
            fs::write(workspace.view_path.join("caseprobe"), b"new\n").unwrap();
        } else {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"case-insensitive-apfs\",\"reason\":\"test volume is case-sensitive\"}}"
            );
        }
        let changes = select_backend(Some(BackendKind::MacClone))
            .unwrap()
            .changes(&workspace, true, false)
            .unwrap();
        assert_eq!(
            changes
                .iter()
                .filter(|change| {
                    change.path.as_os_str().as_bytes() == nfc.as_bytes()
                        || change.path.as_os_str().as_bytes() == nfd.as_bytes()
                })
                .count(),
            1
        );
        assert_eq!(
            changes
                .iter()
                .filter(|change| change.path.as_os_str() == non_utf8)
                .count(),
            1
        );
        let merged = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env(
                "FORKSPACE_HOME",
                std::env::var_os("FORKSPACE_HOME").unwrap(),
            )
            .args(["merge", "names", "--no-install", "--quiet"])
            .output()
            .unwrap();
        assert!(
            merged.status.success(),
            "{}",
            String::from_utf8_lossy(&merged.stderr)
        );
        assert_eq!(fs::read(base.path().join(nfc)).unwrap(), b"new\n");
        assert_eq!(fs::read(base.path().join(&non_utf8)).unwrap(), b"new\n");
        assert_eq!(fs::read(base.path().join(&long)).unwrap(), b"new\n");
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
        let error = forkspace::workspace::create(
            base.path().to_path_buf(),
            "ram",
            forkspace::workspace::CreateOptions {
                tmpfs: true,
                ..Default::default()
            },
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
            kill: false,
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
                forkspace::workspace::create(base.path().to_path_buf(), name, Default::default()),
                Err(ForkspaceError::InvalidArgument(_))
            ));
        }
        let long = "x".repeat(256);
        assert!(matches!(
            forkspace::workspace::create(base.path().to_path_buf(), &long, Default::default()),
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
                        .args(["new", "winner", "--from", base_path.to_str().unwrap()])
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
            kill: false,
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
            kill: false,
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
    fn merge_round_trip_and_conflict_contract() {
        let (home, base) = setup();
        for index in 0..5 {
            fs::write(base.path().join(format!("edit-{index}")), b"old\n").unwrap();
        }
        fs::write(base.path().join("deleted"), b"old\n").unwrap();
        fs::write(base.path().join("mode"), b"same\n").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "forkspace@example.invalid"],
            vec!["config", "user.name", "Forkspace Tests"],
            vec!["add", "-A"],
            vec!["commit", "--quiet", "-m", "base"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(base.path())
                .status()
                .unwrap()
                .success());
        }
        let workspace = fork(base.path(), "round-trip");
        wait_for_manifest("round-trip");
        thread::sleep(Duration::from_millis(2));
        for index in 0..5 {
            fs::write(
                workspace.view_path.join(format!("edit-{index}")),
                format!("new-{index}\n"),
            )
            .unwrap();
        }
        fs::write(workspace.view_path.join("added-a"), b"a\n").unwrap();
        fs::write(workspace.view_path.join("added-b"), b"b\n").unwrap();
        fs::remove_file(workspace.view_path.join("deleted")).unwrap();
        let mut permissions = fs::metadata(workspace.view_path.join("mode"))
            .unwrap()
            .permissions();
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(0o755);
        fs::set_permissions(workspace.view_path.join("mode"), permissions).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["merge", "round-trip", "--commit", "--drop", "--no-install"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!State::load().unwrap().workspaces.contains_key("round-trip"));
        assert!(!base.path().join("deleted").exists());
        assert!(base.path().join("added-a").exists());
        let commits = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(base.path())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&commits.stdout).trim(), "2");

        let workspace = fork(base.path(), "conflict-merge");
        wait_for_manifest("conflict-merge");
        fs::write(base.path().join("edit-0"), b"base moved\n").unwrap();
        fs::write(workspace.view_path.join("edit-0"), b"workspace moved\n").unwrap();
        let refused = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["merge", "conflict-merge", "--no-install"])
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(5));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("edit-0"));
        let forced = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args([
                "merge",
                "conflict-merge",
                "--overwrite-base",
                "--no-install",
            ])
            .output()
            .unwrap();
        assert!(forced.status.success());
        assert_eq!(
            fs::read(base.path().join("edit-0")).unwrap(),
            b"workspace moved\n"
        );
        assert!(State::load()
            .unwrap()
            .workspaces
            .contains_key("conflict-merge"));
    }

    #[test]
    #[serial]
    fn merge_on_exit_with_non_tty_stdin_keeps_workspace() {
        let (home, base) = setup();
        fork(base.path(), "unattended");
        wait_for_manifest("unattended");
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args([
                "enter",
                "unattended",
                "--merge-on-exit",
                "--",
                "sh",
                "-c",
                "printf changed > README.md",
            ])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(fs::read(base.path().join("README.md")).unwrap(), b"base\n");
        assert!(State::load().unwrap().workspaces.contains_key("unattended"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("keeping workspace"));
    }

    #[test]
    #[serial]
    fn stacked_clone_is_snapshot_cumulative_and_merges_to_parent() {
        let (home, base) = setup();
        fs::write(base.path().join("a"), b"base-a\n").unwrap();
        fs::write(base.path().join("c"), b"base-c\n").unwrap();
        let step1 = fork(base.path(), "step-1");
        wait_for_manifest("step-1");
        fs::write(step1.view_path.join("a"), b"parent-a\n").unwrap();
        let step2 = fork(Path::new("step-1"), "step-2");
        wait_for_manifest("step-2");
        assert_eq!(fs::read(step2.view_path.join("a")).unwrap(), b"parent-a\n");
        fs::write(step2.view_path.join("b"), b"child-b\n").unwrap();
        fs::write(step1.view_path.join("c"), b"late-parent-c\n").unwrap();
        assert_eq!(fs::read(step2.view_path.join("c")).unwrap(), b"base-c\n");

        let cumulative = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["diff", "step-2", "--cumulative", "--json"])
            .output()
            .unwrap();
        assert!(cumulative.status.success());
        let json: serde_json::Value = serde_json::from_slice(&cumulative.stdout).unwrap();
        let paths: Vec<_> = json["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| change["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"a"));
        assert!(paths.contains(&"b"));
        assert!(!paths.contains(&"c"));

        let merged = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["merge", "step-2", "--no-install"])
            .output()
            .unwrap();
        assert!(merged.status.success());
        assert_eq!(fs::read(step1.view_path.join("b")).unwrap(), b"child-b\n");
        assert!(!base.path().join("b").exists());

        let conflict = fork(Path::new("step-1"), "step-conflict");
        wait_for_manifest("step-conflict");
        fs::write(step1.view_path.join("a"), b"parent-later\n").unwrap();
        fs::write(conflict.view_path.join("a"), b"child-later\n").unwrap();
        let refused = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["merge", "step-conflict", "--no-install"])
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(5));
        assert_eq!(
            fs::read(step1.view_path.join("a")).unwrap(),
            b"parent-later\n"
        );

        commands::drop::exec(DropArgs {
            name: Some("step-1".into()),
            kill: false,
            force: true,
            all: false,
            glob: false,
        })
        .unwrap();
        assert!(!manifest::manifest_path("step-1").exists());
        assert!(!manifest::manifest_path("step-2").exists());
        assert!(!manifest::manifest_path("step-conflict").exists());
    }

    #[test]
    #[serial]
    fn run_switch_proxy_hot_reload_and_drop_supervision() {
        if !command_exists("python3") {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"python3\",\"reason\":\"dev-server fixture unavailable\"}}"
            );
            return;
        }
        let (home, base) = setup();
        let server = r#"import http.server, os
class H(http.server.BaseHTTPRequestHandler):
  def do_GET(self):
    data=open('served.txt','rb').read()
    self.send_response(200); self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data)
  def log_message(self,*args): pass
http.server.ThreadingHTTPServer(('127.0.0.1',int(os.environ['PORT'])),H).serve_forever()
"#;
        fs::write(base.path().join("server.py"), server).unwrap();
        fs::write(base.path().join("served.txt"), b"base").unwrap();
        let a = fork(base.path(), "dev-a");
        let b = fork(base.path(), "dev-b");
        wait_for_manifest("dev-a");
        wait_for_manifest("dev-b");
        fs::write(a.view_path.join("served.txt"), b"alpha").unwrap();
        fs::write(b.view_path.join("served.txt"), b"beta").unwrap();

        for name in ["dev-a", "dev-b"] {
            let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
                .env("FORKSPACE_HOME", home.path())
                .args(["run", name, "--port", "6200", "--", "python3", "server.py"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn body() -> Option<Vec<u8>> {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", 4000)).ok()?;
            stream
                .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .ok()?;
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).ok()?;
            bytes
                .split(|byte| *byte == b'\n')
                .next_back()
                .map(<[u8]>::to_vec)
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while body().as_deref() != Some(b"beta") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["switch", "dev-a"])
            .output()
            .unwrap();
        assert_eq!(body().as_deref(), Some(&b"alpha"[..]));
        fs::write(a.view_path.join("served.txt"), b"alpha-reloaded").unwrap();
        assert_eq!(body().as_deref(), Some(&b"alpha-reloaded"[..]));

        let state = State::load().unwrap();
        let proxy = state.proxy.as_ref().unwrap();
        unsafe { libc::kill(proxy.proxy_pid.unwrap() as i32, libc::SIGSTOP) };
        unsafe { libc::kill(proxy.proxy_pid.unwrap() as i32, libc::SIGCONT) };
        assert_eq!(body().as_deref(), Some(&b"alpha-reloaded"[..]));

        let b_process = state.workspaces["dev-b"].process.clone().unwrap();
        commands::drop::exec(DropArgs {
            name: Some("dev-b".into()),
            kill: false,
            force: false,
            all: false,
            glob: false,
        })
        .unwrap();
        assert!(!forkspace::process::watch::is_alive(
            b_process.pid,
            b_process.identity.as_deref()
        ));
        let stopped = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["run", "dev-a", "--stop"])
            .output()
            .unwrap();
        assert!(stopped.status.success());
        assert!(forkspace::process::watch::terminate_group(
            proxy.proxy_pid.unwrap(),
            proxy.proxy_identity.as_deref(),
        ));
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
                "new",
                "interrupted",
                "--from",
                base.path().to_str().unwrap(),
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
    fn stopping_and_resuming_at_the_clone_intent_boundary_converges() {
        let (home, base) = setup();
        for index in 0..100_000 {
            let directory = base.path().join(format!("sleep/{:03}", index / 1000));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(format!("{index:06}")), b"x").unwrap();
        }
        let before = hash_tree(base.path());
        let mut child = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args([
                "new",
                "sleep-resume",
                "--from",
                base.path().to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let state_path = home.path().join("state.json");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stopped = false;
        while Instant::now() < deadline {
            let has_intent = fs::read(&state_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|state| state["pending_creates"].as_array().cloned())
                .is_some_and(|pending| !pending.is_empty());
            if has_intent && unsafe { libc::kill(child.id() as i32, libc::SIGSTOP) } == 0 {
                stopped = true;
                break;
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        if !stopped {
            let _ = child.wait();
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"sleep-mid-clone\",\"reason\":\"clone completed before the stop window\"}}"
            );
            return;
        }
        thread::sleep(Duration::from_millis(100));
        assert!(!State::load()
            .unwrap()
            .workspaces
            .contains_key("sleep-resume"));
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGCONT) }, 0);
        assert!(child.wait().unwrap().success());
        let state = State::load().unwrap();
        assert!(state.workspaces.contains_key("sleep-resume"));
        assert_eq!(hash_tree(base.path()), before);
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
            unsafe { std::env::set_var("FORKSPACE_HOME", &home) };
            let error = select_backend(None)
                .unwrap()
                .preflight(Path::new(&base))
                .unwrap_err();
            assert_preflight_golden("non-apfs", &error);
            assert_eq!(error.exit_code(), 7);
            let message = error.to_string();
            assert!(message.contains("APFS clones are unavailable"));
            assert!(!message.contains('\n'));
            assert!(!message.contains("FUSE"));
            assert!(!message.contains("brew"));

            let copied = Command::new(env!("CARGO_BIN_EXE_forkspace"))
                .env("FORKSPACE_HOME", &home)
                .args([
                    "new",
                    "non-apfs-copy",
                    "--from",
                    Path::new(&base).to_str().unwrap(),
                    "--copy",
                ])
                .output()
                .unwrap();
            assert!(
                copied.status.success(),
                "{}",
                String::from_utf8_lossy(&copied.stderr)
            );
            let stderr = String::from_utf8_lossy(&copied.stderr);
            assert!(stderr.contains("full copy"));
            assert!(stderr.contains("APFS"));
            assert!(!stderr.contains("fuse"));
        } else {
            eprintln!(
                "FORKSPACE_TEST_SKIP {{\"capability\":\"non-apfs\",\"reason\":\"set FORKSPACE_TEST_NON_APFS_BASE and FORKSPACE_TEST_NON_APFS_HOME\"}}"
            );
        }
    }
}
