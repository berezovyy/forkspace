use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{CommandFactory, Parser};
use forkspace::error::ForkspaceError;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    name: String,
    args: Vec<String>,
    #[serde(default)]
    state_fixture: Option<String>,
    #[serde(default)]
    setup: Option<String>,
}

fn setup_case(case: &Case, home: &Path) {
    if case.setup.as_deref() == Some("compare-three") {
        let base = home.join("compare-base");
        fs::create_dir_all(&base).unwrap();
        for name in ["all", "partial", "delete"] {
            fs::write(base.join(name), "old\n").unwrap();
        }
        let mut workspaces = serde_json::Map::new();
        for name in ["a", "b", "c"] {
            let root = home.join(name);
            for directory in [root.join("upper"), root.join("work"), root.join("merged")] {
                fs::create_dir_all(directory).unwrap();
            }
            fs::write(root.join("upper/all"), name).unwrap();
            if name != "c" {
                fs::write(root.join("upper/partial"), name).unwrap();
            }
            if name == "a" {
                fs::write(root.join("upper/only-a"), "a").unwrap();
            }
            if name == "c" {
                fs::write(root.join("upper/.wh.delete"), "").unwrap();
            }
            workspaces.insert(
                name.to_string(),
                serde_json::json!({
                    "name": name,
                    "base": base,
                    "parent": null,
                    "created": "2025-01-01T00:00:00Z",
                    "session": null,
                    "view_path": root.join("merged"),
                    "fork_time_ns": 1,
                    "backend_kind": "linux",
                    "backend_state": {
                        "kind": "linux",
                        "upper": root.join("upper"),
                        "work": root.join("work"),
                        "merged": root.join("merged"),
                        "tmpfs": false
                    }
                }),
            );
        }
        fs::write(
            home.join("state.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 2,
                "workspaces": workspaces,
                "proxy": null
            }))
            .unwrap(),
        )
        .unwrap();
        return;
    }
    if case.setup.as_deref() == Some("copy-small") {
        fs::create_dir_all(home.join("base")).unwrap();
        fs::write(home.join("base/file"), "x\n").unwrap();
        return;
    }
    if case.setup.as_deref() != Some("diff-class") {
        return;
    }
    let base = home.join("base");
    let upper = home.join("golden/upper");
    let work = home.join("golden/work");
    let merged = home.join("golden/merged");
    for directory in [&base, &upper, &work, &merged, &upper.join(".git")] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(base.join("source.rs"), "old\n").unwrap();
    fs::write(base.join(".gitignore"), "Cargo.lock\n").unwrap();
    fs::write(upper.join("source.rs"), "new\n").unwrap();
    fs::write(upper.join("Cargo.lock"), "lock\n").unwrap();
    fs::write(upper.join(".git/index"), "internal\n").unwrap();
    let state = serde_json::json!({
        "version": 2,
        "workspaces": {
            "golden": {
                "name": "golden",
                "base": base,
                "parent": null,
                "created": "2025-01-01T00:00:00Z",
                "session": null,
                "view_path": merged,
                "fork_time_ns": 1,
                "backend_kind": "linux",
                "backend_state": {
                    "kind": "linux",
                    "upper": upper,
                    "work": work,
                    "merged": merged,
                    "tmpfs": false
                }
            }
        },
        "proxy": null
    });
    fs::write(
        home.join("state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli-contract")
}

#[test]
fn golden_cli_contract() {
    let cases: Vec<Case> =
        serde_json::from_slice(&fs::read(fixture_dir().join("cases.json")).unwrap()).unwrap();
    assert!(
        cases.len() >= 30,
        "the CLI contract must cover about 30 invocations"
    );

    for case in cases {
        let home = tempfile::tempdir().unwrap();
        if let Some(fixture) = &case.state_fixture {
            fs::copy(fixture_dir().join(fixture), home.path().join("state.json")).unwrap();
        }
        setup_case(&case, home.path());

        let args: Vec<_> = case
            .args
            .iter()
            .map(|arg| arg.replace("<FORKSPACE_HOME>", &home.path().display().to_string()))
            .collect();
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .args(&args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("FORKSPACE_HOME", home.path())
            .env("TZ", "UTC")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();

        let prefix = fixture_dir().join(&case.name);
        let stdout = if case.setup.is_some() {
            String::from_utf8(output.stdout)
                .unwrap()
                .replace(&home.path().display().to_string(), "<FORKSPACE_HOME>")
                .into_bytes()
        } else {
            output.stdout
        };
        assert_eq!(
            stdout,
            fs::read(prefix.with_extension("stdout")).unwrap(),
            "stdout changed for {}",
            case.name
        );
        let stderr = if case.setup.as_deref() == Some("copy-small") {
            let value = String::from_utf8(output.stderr).unwrap();
            let value =
                if let (Some(start), Some(end)) = (value.find(" bytes in "), value.find(" ms);")) {
                    format!(
                        "{} bytes in <TIME> ms);{}",
                        &value[..start],
                        &value[end + 5..]
                    )
                } else {
                    value
                };
            value.into_bytes()
        } else {
            output.stderr
        };
        assert_eq!(
            stderr,
            fs::read(prefix.with_extension("stderr")).unwrap(),
            "stderr changed for {}",
            case.name
        );
        let expected_code: i32 = fs::read_to_string(prefix.with_extension("code"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "exit code changed for {}",
            case.name
        );
    }
}

#[test]
fn every_exit_code_has_a_contract_case() {
    assert_eq!(
        forkspace::cli::Cli::try_parse_from(["forkspace", "--help"])
            .err()
            .unwrap()
            .exit_code(),
        0
    );
    assert_eq!(ForkspaceError::InvalidArgument("x".into()).exit_code(), 1);
    assert_eq!(
        forkspace::cli::Cli::try_parse_from(["forkspace"])
            .err()
            .unwrap()
            .exit_code(),
        2
    );
    assert_eq!(ForkspaceError::WorkspaceNotFound("x".into()).exit_code(), 3);
    assert_eq!(ForkspaceError::WorkspaceExists("x".into()).exit_code(), 4);
    assert_eq!(
        ForkspaceError::HasChildren {
            workspace: "x".into(),
            children: vec!["y".into()],
        }
        .exit_code(),
        5
    );
    assert_eq!(
        ForkspaceError::MergeConflict {
            paths: vec!["file".into()]
        }
        .exit_code(),
        5
    );
    assert_eq!(
        ForkspaceError::CapabilityUnsupported {
            diagnosis: "x".into(),
            remedy: "y".into(),
        }
        .exit_code(),
        6
    );
    assert_eq!(
        ForkspaceError::BackendUnavailable {
            diagnosis: "x".into(),
            remedy: "y".into(),
        }
        .exit_code(),
        7
    );
}

#[test]
fn handwritten_zsh_completions_match_visible_cli_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let command_block = text
        .split_once("commands=(")
        .unwrap()
        .1
        .split_once("\n    )")
        .unwrap()
        .0;
    let completed: std::collections::BTreeSet<_> = command_block
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let value = line.strip_prefix('\'')?.strip_suffix('\'')?;
            let (command, _) = value.split_once(':')?;
            (!command.starts_with('-')).then(|| command.to_string())
        })
        .collect();
    let visible: std::collections::BTreeSet<_> = forkspace::cli::Cli::command()
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().to_string())
        .collect();
    assert_eq!(completed, visible);
}
