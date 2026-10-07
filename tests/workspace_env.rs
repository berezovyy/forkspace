mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serial_test::serial;

fn forkspace(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home)
        .env_remove("COMPOSE_PROJECT_NAME")
        .envs(env.iter().copied())
        .args(args)
        .output()
        .unwrap()
}

/// A minimal stand-in for `python -m venv .venv` created inside `project`.
fn fake_venv(project: &Path) {
    let venv = project.join(".venv");
    fs::create_dir_all(venv.join("bin")).unwrap();
    fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    fs::write(venv.join("bin/python"), "").unwrap();
    fs::write(
        venv.join("bin/activate"),
        format!("VIRTUAL_ENV='{}'\nexport VIRTUAL_ENV\n", venv.display()),
    )
    .unwrap();
    fs::write(
        venv.join("bin/pip"),
        format!("#!{}/bin/python\nimport pip\n", venv.display()),
    )
    .unwrap();
}

#[test]
#[serial]
fn new_warns_about_a_copied_venv_that_points_at_the_base() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fake_venv(base.path());
    let base_path = base.path().canonicalize().unwrap();

    let output = forkspace(
        home.path(),
        &[
            "new",
            "venv-ws",
            "--from",
            base_path.to_str().unwrap(),
            "--copy",
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "warning: .venv is a Python virtual environment for {}/.venv",
            base_path.display()
        )),
        "{stderr}"
    );
    assert!(stderr.contains("python3 -m venv --clear .venv"), "{stderr}");
}

#[test]
#[serial]
fn new_does_not_warn_about_unrelated_shebangs() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let tool = base.path().join("tools");
    fs::create_dir_all(tool.join("bin")).unwrap();
    fs::write(tool.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    fs::write(tool.join("bin/script"), "#!/usr/bin/env python3\n").unwrap();

    let output = forkspace(
        home.path(),
        &[
            "new",
            "plain-ws",
            "--from",
            base.path().to_str().unwrap(),
            "--copy",
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("virtual environment"));
}

#[test]
#[serial]
fn enter_gives_each_workspace_its_own_compose_project() {
    let home = common::setup_test_env();
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("My Shop");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("compose.yaml"), "services: {}\n").unwrap();
    let new = forkspace(
        home.path(),
        &["new", "Agent.1", "--from", base.to_str().unwrap(), "--copy"],
        &[],
    );
    assert!(
        new.status.success(),
        "{}",
        String::from_utf8_lossy(&new.stderr)
    );

    let print = [
        "enter",
        "Agent.1",
        "--",
        "sh",
        "-c",
        "printf %s \"$COMPOSE_PROJECT_NAME\"",
    ];
    let output = forkspace(home.path(), &print, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("my-shop-agent-1"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let output = forkspace(home.path(), &print, &[("COMPOSE_PROJECT_NAME", "mine")]);
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("mine"));
}
