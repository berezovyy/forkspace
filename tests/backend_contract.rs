use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use forkspace::backend::{
    select_backend, BackendKind, BackendState, CreateReq, DestroyMode, ViewInfo, WorkspaceBackend,
};
use forkspace::capabilities::Capabilities;
use forkspace::diff::FileChange;
use forkspace::error::{ForkspaceError, Result};
use forkspace::workspace::Workspace;
use serial_test::serial;

const FAKE_CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: true,
    network_namespace: false,
    instant_fork: false,
    sealed_parent: false,
};

struct FakeBackend {
    creates: Arc<AtomicUsize>,
    destroys: Arc<AtomicUsize>,
}

impl WorkspaceBackend for FakeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::MacClone
    }

    fn capabilities(&self) -> &Capabilities {
        &FAKE_CAPABILITIES
    }

    fn preflight(&self, base: &Path) -> Result {
        if base.is_dir() {
            Ok(())
        } else {
            Err(ForkspaceError::InvalidArgument(
                "fake base must be a directory".into(),
            ))
        }
    }

    fn create(&self, req: &CreateReq) -> Result<ViewInfo> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        let view = req.base.join(&req.name);
        fs::create_dir_all(&view).unwrap();
        Ok(ViewInfo {
            view_path: view,
            fork_time_ns: 42,
            backend_state: BackendState::MacClone {
                unknown_fields: serde_json::Map::new(),
            },
        })
    }

    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result {
        self.destroys.fetch_add(1, Ordering::SeqCst);
        fs::remove_dir_all(&ws.view_path).unwrap();
        Ok(())
    }

    fn view_path(&self, ws: &Workspace) -> PathBuf {
        ws.view_path.clone()
    }

    fn changes(&self, _ws: &Workspace) -> Result<Vec<FileChange>> {
        Ok(Vec::new())
    }

    fn shell_env(&self, ws: &Workspace) -> Vec<(String, String)> {
        vec![("FORKSPACE_VIEW".into(), ws.view_path.display().to_string())]
    }
}

#[test]
fn fake_backend_passes_workspace_contract() {
    let root = tempfile::tempdir().unwrap();
    let creates = Arc::new(AtomicUsize::new(0));
    let destroys = Arc::new(AtomicUsize::new(0));
    let backend = FakeBackend {
        creates: creates.clone(),
        destroys: destroys.clone(),
    };
    let req = CreateReq {
        name: "view".into(),
        base: root.path().to_path_buf(),
        tmpfs: false,
        tmpfs_size: None,
    };

    backend.preflight(root.path()).unwrap();
    let view = backend.create(&req).unwrap();
    let workspace = Workspace::from_view(
        "view".into(),
        root.path().to_path_buf(),
        None,
        None,
        backend.kind(),
        view,
    );

    assert_eq!(backend.kind(), BackendKind::MacClone);
    assert!(backend.capabilities().tmpfs_backing);
    assert_eq!(backend.view_path(&workspace), root.path().join("view"));
    assert!(backend.changes(&workspace).unwrap().is_empty());
    assert_eq!(
        backend.shell_env(&workspace),
        vec![(
            "FORKSPACE_VIEW".into(),
            root.path().join("view").display().to_string()
        )]
    );
    backend.destroy(&workspace, DestroyMode::Normal).unwrap();
    assert_eq!(creates.load(Ordering::SeqCst), 1);
    assert_eq!(destroys.load(Ordering::SeqCst), 1);
}

#[test]
#[serial]
fn backend_selection_is_automatic_and_debug_overridable() {
    unsafe { std::env::remove_var("FORKSPACE_BACKEND") };
    let selected = select_backend(None).unwrap();
    assert_eq!(selected.kind(), BackendKind::Linux);

    unsafe { std::env::set_var("FORKSPACE_BACKEND", "linux") };
    assert_eq!(select_backend(None).unwrap().kind(), BackendKind::Linux);

    unsafe { std::env::set_var("FORKSPACE_BACKEND", "invalid") };
    let error = select_backend(None).err().unwrap();
    assert!(matches!(error, ForkspaceError::BackendUnavailable { .. }));
    assert_eq!(error.exit_code(), 7);
    unsafe { std::env::remove_var("FORKSPACE_BACKEND") };
}

#[test]
#[serial]
fn recorded_other_platform_backend_is_unavailable() {
    unsafe { std::env::remove_var("FORKSPACE_BACKEND") };
    let error = select_backend(Some(BackendKind::MacClone)).err().unwrap();
    assert_eq!(error.exit_code(), 7);
    let message = error.to_string();
    assert!(message.contains("mac-clone"));
    assert!(message.contains("macOS"));
}

#[test]
fn command_layer_has_no_linux_layout_or_platform_diagnostics() {
    fn references_field(source: &str, field: &str) -> bool {
        source.match_indices(field).any(|(index, _)| {
            source[index + field.len()..]
                .chars()
                .next()
                .is_none_or(|next| !next.is_ascii_alphanumeric() && next != '_')
        })
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands");
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("overlay::"),
            "{} references overlay",
            path.display()
        );
        for field in [".upper", ".work", ".merged"] {
            assert!(
                !references_field(&source, field),
                "{} references {field}",
                path.display()
            );
        }
        for platform_text in [
            "overlay",
            "fuse-overlayfs",
            "fusermount",
            "clonefile",
            "APFS",
            "brew install",
        ] {
            assert!(
                !source.contains(platform_text),
                "{} contains platform diagnostic",
                path.display()
            );
        }
    }

    let linux =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backend/linux.rs"))
            .unwrap();
    assert!(!linux.contains("brew install"));
    assert!(!linux.contains("APFS"));
}
