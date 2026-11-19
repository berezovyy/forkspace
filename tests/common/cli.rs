//! A scratch home and project driven through the `forkspace` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use forkspace::state::State;
use forkspace::workspace::Workspace;

pub struct Env {
    pub home: tempfile::TempDir,
    pub root: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        let env = Self {
            home: super::setup_test_env(),
            root: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(env.project()).unwrap();
        fs::create_dir(env.outside()).unwrap();
        env
    }

    pub fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    /// A directory outside every tree forkspace uses.
    pub fn outside(&self) -> PathBuf {
        self.root.path().join("outside")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forkspace"));
        command
            .env("FORKSPACE_HOME", self.home.path())
            // A child run as a background manifest builder would not run the command.
            .env_remove("FORKSPACE_INTERNAL_MANIFEST_WORKER")
            .current_dir(self.project())
            .args(args);
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    pub fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}{}",
            stdout(&output),
            stderr(&output)
        );
        output
    }

    pub fn fork_copy(&self, name: &str) {
        let project = self.project();
        self.ok(&["new", name, "--from", project.to_str().unwrap(), "--copy"]);
    }

    pub fn workspace(&self, name: &str) -> Workspace {
        State::load()
            .unwrap()
            .require_workspace(name)
            .unwrap()
            .clone()
    }

    pub fn view(&self, name: &str) -> PathBuf {
        self.workspace(name).view_path
    }
}

pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
