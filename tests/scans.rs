//! Snapshot scans skip directories whose changes are never transferred, without changing
//! what `diff`, `merge`, `pull`, and `ls` report.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
}

impl Env {
    /// A project with sources, an ignored build directory, `.env`, and a `node_modules` tree.
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
        };
        let project = env.project();
        for directory in ["src", "dist", "node_modules/pkg/lib"] {
            fs::create_dir_all(project.join(directory)).unwrap();
        }
        fs::write(project.join(".gitignore"), "dist/\n.env\n").unwrap();
        fs::write(project.join(".env"), "TOKEN=1\n").unwrap();
        fs::write(project.join("src/app.js"), "app\n").unwrap();
        fs::write(project.join("dist/app.js"), "built\n").unwrap();
        fs::write(project.join("node_modules/pkg/lib/index.js"), "pkg\n").unwrap();
        fs::write(project.join("node_modules/pkg/package.json"), "{}\n").unwrap();
        let output = env.run(&["new", "ws", "--from", project.to_str().unwrap(), "--copy"]);
        assert!(output.status.success());
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn view(&self) -> PathBuf {
        self.home.path().join("workspaces/ws")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", "copy")
            .env("NO_COLOR", "1")
            .args(args)
            .output()
            .unwrap()
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

    fn changed(&self, json_args: &[&str]) -> usize {
        let listed: serde_json::Value = serde_json::from_str(&self.ok(json_args)).unwrap();
        listed["changes"].as_array().unwrap().len()
    }
}

/// Changes inside and outside `node_modules`, the ignored `dist/`, and `.env`.
fn edit_everywhere(env: &Env) {
    let view = env.view();
    fs::write(view.join("src/app.js"), "app v2\n").unwrap();
    fs::write(view.join("src/new.js"), "new\n").unwrap();
    fs::write(view.join("dist/app.js"), "rebuilt\n").unwrap();
    fs::write(view.join("node_modules/pkg/lib/index.js"), "patched\n").unwrap();
    fs::write(view.join("node_modules/pkg/lib/extra.js"), "extra\n").unwrap();
    fs::remove_file(view.join("node_modules/pkg/package.json")).unwrap();
}

#[test]
fn merge_ls_and_diff_agree_while_node_modules_is_skipped() {
    let env = Env::new();
    edit_everywhere(&env);

    let plan = env.ok(&["merge", "ws", "--dry-run"]);
    assert!(plan.contains("merge plan for 'ws': 2 files"), "{plan}");
    assert!(
        plan.contains("modify src/app.js") && plan.contains("add src/new.js"),
        "{plan}"
    );
    assert!(
        !plan.contains("node_modules") && !plan.contains("dist/"),
        "{plan}"
    );

    assert_eq!(env.changed(&["diff", "ws", "--json"]), 2);
    let listed: serde_json::Value = serde_json::from_str(&env.ok(&["ls", "--json"])).unwrap();
    assert_eq!(listed[0]["name"], "ws");
    let table = env.ok(&["ls"]);
    assert!(table.contains("2 files"), "{table}");

    // Asking for ignored paths still walks everything.
    assert_eq!(env.changed(&["diff", "ws", "--json", "--no-gitignore"]), 6);
    let everything = env.ok(&["ls", "--no-gitignore"]);
    assert!(everything.contains("6 files"), "{everything}");
}

#[test]
fn pull_still_sees_ignored_files_that_transfer() {
    let env = Env::new();
    // Both sides change `.env` (ignored, but transferable): a whole-file decision is needed.
    fs::write(env.view().join(".env"), "TOKEN=workspace\0\n").unwrap();
    fs::write(env.project().join(".env"), "TOKEN=project\0\n").unwrap();
    // The project changes `node_modules` and `dist/`; only `dist/` transfers.
    fs::write(
        env.project().join("node_modules/pkg/lib/index.js"),
        "upgraded\n",
    )
    .unwrap();
    fs::write(env.project().join("dist/app.js"), "built again\n").unwrap();

    let refused = env.run(&["pull", "ws", "--dry-run", "--no-install"]);
    assert_eq!(refused.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&refused.stderr).contains(".env"));

    let plan = env.ok(&["pull", "ws", "--dry-run", "--theirs", "--no-install"]);
    assert!(plan.contains("update dist/app.js"), "{plan}");
    assert!(plan.contains("update .env"), "{plan}");
    assert!(!plan.contains("node_modules"), "{plan}");
}

#[test]
fn repeated_scans_give_identical_sorted_output() {
    let env = Env::new();
    edit_everywhere(&env);
    let first = env.ok(&["diff", "ws", "--json", "--no-gitignore"]);
    for _ in 0..5 {
        assert_eq!(env.ok(&["diff", "ws", "--json", "--no-gitignore"]), first);
    }
    let listed: serde_json::Value = serde_json::from_str(&first).unwrap();
    let paths: Vec<&str> = listed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}
