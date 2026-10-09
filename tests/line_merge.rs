//! Line-level merge and pull: files both sides changed are combined like `git merge-file`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    backend: &'static str,
    path: Option<String>,
}

impl Env {
    fn new(backend: &'static str) -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
            backend,
            path: None,
        };
        fs::create_dir(env.project()).unwrap();
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn view(&self, name: &str) -> PathBuf {
        if self.backend == "linux" {
            self.home.path().join(name).join("merged")
        } else {
            self.home.path().join("workspaces").join(name)
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forkspace"));
        command
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", self.backend)
            .env("NO_COLOR", "1")
            .args(args);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        command.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fork(&self, name: &str) {
        let project = self.project();
        self.ok(&["new", name, "--from", project.to_str().unwrap(), "--copy"]);
    }

    fn read(&self, path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.path = None;
        let _ = self.run(&["drop", "--all"]);
    }
}

fn numbered(lines: usize) -> String {
    (1..=lines).map(|n| format!("line {n}\n")).collect()
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{from:?} not in {}", path.display());
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

fn exit_code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn different_lines_reach_the_project_from_both_workspaces(backend: &'static str) {
    let env = Env::new(backend);
    fs::write(env.project().join("app.txt"), numbered(20)).unwrap();
    for name in ["ws1", "ws2", "ws3"] {
        let project = env.project();
        let mut args = vec!["new", name, "--from", project.to_str().unwrap()];
        if backend == "copy" {
            args.push("--copy");
        }
        env.ok(&args);
    }
    edit(&env.view("ws1").join("app.txt"), "line 2\n", "line 2 ws1\n");
    edit(
        &env.view("ws2").join("app.txt"),
        "line 18\n",
        "line 18 ws2\n",
    );
    edit(
        &env.view("ws3").join("app.txt"),
        "line 10\n",
        "line 10 ws3\n",
    );

    env.ok(&["merge", "ws1", "--quiet", "--no-install"]);
    // Merging straight away combines the edits...
    let plan = env.ok(&["merge", "ws2", "--dry-run"]);
    assert!(plan.contains("merged line by line: app.txt"), "{plan}");
    env.ok(&["merge", "ws2", "--quiet", "--no-install"]);
    // ...and so does pulling first, then merging.
    let pulled = env.ok(&["pull", "ws3", "--no-install"]);
    assert!(pulled.contains("merged line by line: app.txt"), "{pulled}");
    let view = env.read(&env.view("ws3").join("app.txt"));
    assert!(view.contains("line 2 ws1\n") && view.contains("line 18 ws2\n"));
    env.ok(&["merge", "ws3", "--quiet", "--no-install"]);

    let project = env.read(&env.project().join("app.txt"));
    for expected in ["line 2 ws1\n", "line 10 ws3\n", "line 18 ws2\n"] {
        assert!(
            project.contains(expected),
            "missing {expected:?} in {project}"
        );
    }
}

#[test]
fn different_lines_merge_on_the_copy_backend() {
    different_lines_reach_the_project_from_both_workspaces("copy");
}

#[test]
fn different_lines_merge_on_a_sealed_overlay() {
    skip_without_overlay!();
    different_lines_reach_the_project_from_both_workspaces("linux");
}

#[test]
fn same_lines_get_markers_in_the_workspace_and_never_reach_the_project() {
    let env = Env::new("copy");
    fs::write(env.project().join("app.txt"), numbered(5)).unwrap();
    env.fork("ws1");
    env.fork("ws2");
    edit(&env.view("ws1").join("app.txt"), "line 3\n", "line 3 ws1\n");
    edit(&env.view("ws2").join("app.txt"), "line 3\n", "line 3 ws2\n");
    env.ok(&["merge", "ws1", "--quiet", "--no-install"]);
    let project_before = env.read(&env.project().join("app.txt"));

    let refused = env.run(&["merge", "ws2", "--quiet", "--no-install"]);
    assert_eq!(exit_code(&refused), Some(5));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("forkspace pull ws2"));
    assert_eq!(env.read(&env.project().join("app.txt")), project_before);

    let pulled = env.run(&["pull", "ws2", "--no-install"]);
    assert_eq!(exit_code(&pulled), Some(5));
    let marked = env.read(&env.view("ws2").join("app.txt"));
    for marker in [
        "<<<<<<< workspace\n",
        "||||||| fork\n",
        "=======\n",
        ">>>>>>> project\n",
    ] {
        assert!(marked.contains(marker), "missing {marker:?} in {marked}");
    }

    let blocked = env.run(&["merge", "ws2", "--quiet", "--no-install"]);
    assert_eq!(exit_code(&blocked), Some(5));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("conflict markers"));
    assert_eq!(env.read(&env.project().join("app.txt")), project_before);

    let resolved = numbered(5).replace("line 3\n", "line 3 ws1 and ws2\n");
    fs::write(env.view("ws2").join("app.txt"), &resolved).unwrap();
    env.ok(&["merge", "ws2", "--quiet", "--no-install"]);
    assert_eq!(env.read(&env.project().join("app.txt")), resolved);
}

#[test]
fn package_json_merges_and_the_lockfile_is_regenerated_not_merged() {
    let mut env = Env::new("copy");
    let manifest = "{\n  \"name\": \"app\",\n  \"dependencies\": {\n    \"a\": \"1.0.0\"\n  },\n  \"devDependencies\": {\n    \"x\": \"1.0.0\"\n  }\n}\n";
    fs::write(env.project().join("package.json"), manifest).unwrap();
    fs::write(env.project().join("package-lock.json"), "{\"lock\": 0}\n").unwrap();
    env.fork("ws1");
    env.fork("ws2");
    edit(
        &env.view("ws1").join("package.json"),
        "\"a\": \"1.0.0\"\n",
        "\"a\": \"1.0.0\",\n    \"b\": \"2.0.0\"\n",
    );
    fs::write(env.view("ws1").join("package-lock.json"), "{\"lock\": 1}\n").unwrap();
    edit(
        &env.view("ws2").join("package.json"),
        "\"x\": \"1.0.0\"\n",
        "\"x\": \"1.0.0\",\n    \"y\": \"3.0.0\"\n",
    );
    fs::write(env.view("ws2").join("package-lock.json"), "{\"lock\": 2}\n").unwrap();
    env.ok(&["merge", "ws1", "--quiet", "--no-install"]);

    // A stand-in `npm` that regenerates the lockfile, so the test needs no network.
    let bin = env.root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("npm"),
        "#!/bin/sh\necho '{\"lock\": \"regenerated\"}' > package-lock.json\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("npm"), fs::Permissions::from_mode(0o755)).unwrap();
    env.path = Some(format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap()
    ));

    let merged = env.ok(&["merge", "ws2"]);
    assert!(
        merged.contains("merged line by line: package.json"),
        "{merged}"
    );
    assert!(merged.contains("keeping the base lockfile"), "{merged}");
    let package = env.read(&env.project().join("package.json"));
    assert!(package.contains("\"b\": \"2.0.0\"") && package.contains("\"y\": \"3.0.0\""));
    assert_eq!(
        env.read(&env.project().join("package-lock.json")),
        "{\"lock\": \"regenerated\"}\n"
    );
}

#[test]
fn binary_files_both_sides_changed_stay_whole_file_conflicts() {
    let env = Env::new("copy");
    fs::write(env.project().join("image.bin"), b"\0base\n").unwrap();
    env.fork("ws1");
    env.fork("ws2");
    fs::write(env.view("ws1").join("image.bin"), b"\0one\n").unwrap();
    fs::write(env.view("ws2").join("image.bin"), b"\0two\n").unwrap();
    env.ok(&["merge", "ws1", "--quiet", "--no-install"]);
    let refused = env.run(&["merge", "ws2", "--quiet", "--no-install"]);
    assert_eq!(exit_code(&refused), Some(5));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("base changed since fork"));
    assert_eq!(
        fs::read(env.project().join("image.bin")).unwrap(),
        b"\0one\n"
    );
}

#[test]
fn without_git_both_sides_changed_is_a_whole_file_conflict() {
    let mut env = Env::new("copy");
    fs::write(env.project().join("app.txt"), numbered(20)).unwrap();
    env.fork("ws1");
    env.fork("ws2");
    edit(&env.view("ws1").join("app.txt"), "line 2\n", "line 2 ws1\n");
    edit(
        &env.view("ws2").join("app.txt"),
        "line 18\n",
        "line 18 ws2\n",
    );
    env.ok(&["merge", "ws1", "--quiet", "--no-install"]);
    let empty = env.root.path().join("empty-bin");
    fs::create_dir(&empty).unwrap();
    env.path = Some(empty.display().to_string());
    let refused = env.run(&["merge", "ws2", "--quiet", "--no-install"]);
    assert_eq!(exit_code(&refused), Some(5));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("base changed since fork"));
}

#[test]
fn an_interrupted_auto_merge_resumes_from_its_journal() {
    use forkspace::sync::journal::{
        self, Fingerprint, MergedStep, OperationJournal, OperationKind, OperationPhase,
    };

    let env = Env::new("copy");
    fs::write(env.project().join("app.txt"), numbered(20)).unwrap();
    env.fork("ws1");
    env.fork("ws2");
    edit(&env.view("ws1").join("app.txt"), "line 2\n", "line 2 ws1\n");
    edit(
        &env.view("ws2").join("app.txt"),
        "line 18\n",
        "line 18 ws2\n",
    );
    env.ok(&["merge", "ws1", "--quiet", "--no-install"]);

    // Record the journal a crash would leave: planned, nothing applied, merge staged.
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(env.home.path().join("state.json")).unwrap()).unwrap();
    let fork_time_ns: u128 = state["workspaces"]["ws2"]["fork_time_ns"]
        .as_u64()
        .unwrap()
        .into();
    let path = env
        .home
        .path()
        .join(format!("journal/merge-ws2-{fork_time_ns}.json"));
    let staged = journal::staging_dir(&path).join("app.txt");
    fs::create_dir_all(staged.parent().unwrap()).unwrap();
    let combined = env
        .read(&env.project().join("app.txt"))
        .replace("line 18\n", "line 18 ws2\n");
    fs::write(&staged, &combined).unwrap();
    let target = env.project().join("app.txt");
    let journaled = OperationJournal {
        op: OperationKind::Merge,
        workspace: "ws2".to_string(),
        source: env.view("ws2"),
        target: env.project(),
        changes: vec![forkspace::changes::FileChange::new(
            PathBuf::from("app.txt"),
            forkspace::changes::ChangeKind::Modified,
        )],
        next: 0,
        phase: OperationPhase::Applying,
        fork_time_ns,
        undo: None,
        merged: vec![MergedStep {
            path: PathBuf::from("app.txt"),
            expected: Fingerprint::of(&target),
        }],
    };
    journal::save(&path, &journaled).unwrap();

    env.ok(&["merge", "ws2", "--resume", "--no-install"]);
    assert_eq!(env.read(&target), combined);
    assert!(!path.exists() && !journal::staging_dir(&path).exists());
}
