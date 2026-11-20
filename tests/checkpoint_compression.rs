//! Checkpoints keep changed files zstd-compressed against their fork-time version
//! (`src/sync/compress.rs`): a small edit of a large file takes a few hundred bytes, and a
//! restore is byte for byte, with the file's permissions and modification time.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::cli::{stdout, write, Env};
use serial_test::serial;

fn source_text() -> String {
    (0..6000)
        .map(|line| {
            format!(
                "line {line}: text that an agent might edit, value {}\n",
                line * 13
            )
        })
        .collect()
}

fn record(env: &Env, workspace: &str, id: u64) -> serde_json::Value {
    let path = env
        .home
        .path()
        .join("checkpoints")
        .join(workspace)
        .join(id.to_string())
        .join("record.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn stored_file(env: &Env, workspace: &str, id: u64, relative: &str) -> PathBuf {
    env.home
        .path()
        .join("checkpoints")
        .join(workspace)
        .join(id.to_string())
        .join("files")
        .join(relative)
}

fn latest_id(env: &Env, workspace: &str) -> u64 {
    let listed: serde_json::Value =
        serde_json::from_slice(&env.ok(&["checkpoints", workspace, "--json"]).stdout).unwrap();
    listed
        .as_array()
        .unwrap()
        .iter()
        .map(|checkpoint| checkpoint["id"].as_u64().unwrap())
        .max()
        .unwrap()
}

fn edit(view: &Path) {
    let text = fs::read_to_string(view.join("src/big.txt")).unwrap();
    let text = text.replacen("line 2000:", "line 2000 (edited):", 1) + "an added line\n";
    fs::write(view.join("src/big.txt"), text).unwrap();
    fs::set_permissions(view.join("src/big.txt"), fs::Permissions::from_mode(0o640)).unwrap();
}

#[test]
#[serial]
fn a_small_edit_is_kept_as_a_small_delta_and_restored_exactly() {
    let env = Env::new();
    write(&env.project().join("src/big.txt"), &source_text());
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let random: Vec<u8> = (0..20_000)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 32) as u8
        })
        .collect();
    fs::write(env.project().join("noise.bin"), &random).unwrap();
    {
        let name = "default";
        env.ok(&["new", name]);
        let view = env.view(name);
        edit(&view);
        let noise: Vec<u8> = random.iter().map(|byte| byte ^ 0x5a).collect();
        fs::write(view.join("noise.bin"), &noise).unwrap();
        let expected = fs::read(view.join("src/big.txt")).unwrap();
        let expected_meta = fs::metadata(view.join("src/big.txt")).unwrap();

        env.ok(&["checkpoint", name, "-m", "edited"]);
        let id = latest_id(&env, name);
        let entries = record(&env, name, id)["entries"].clone();
        let big = entries
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == "src/big.txt")
            .unwrap();
        assert!(big["stored"]["reference"].is_string(), "{name}: {big}");
        let kept = fs::metadata(stored_file(&env, name, id, "src/big.txt"))
            .unwrap()
            .len();
        assert!(
            kept < 2_000,
            "{name}: {kept} bytes kept for {} bytes",
            expected.len()
        );
        // Incompressible content is copied as it is.
        let noise_entry = entries
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == "noise.bin")
            .unwrap();
        assert!(noise_entry.get("stored").is_none(), "{name}: {noise_entry}");

        fs::write(view.join("src/big.txt"), "overwritten\n").unwrap();
        fs::remove_file(view.join("noise.bin")).unwrap();
        env.ok(&["restore", name, &id.to_string(), "--no-install"]);
        assert_eq!(
            fs::read(view.join("src/big.txt")).unwrap(),
            expected,
            "{name}"
        );
        let restored = fs::metadata(view.join("src/big.txt")).unwrap();
        assert_eq!(restored.permissions().mode() & 0o7777, 0o640, "{name}");
        assert_eq!(
            restored.modified().unwrap(),
            expected_meta.modified().unwrap(),
            "{name}"
        );
        assert_eq!(fs::read(view.join("noise.bin")).unwrap(), noise, "{name}");
    }
}

#[test]
#[serial]
fn a_checkpoint_restores_after_pull_moved_the_fork_point() {
    let env = Env::new();
    write(&env.project().join("src/big.txt"), &source_text());
    write(&env.project().join("other.txt"), "other\n");
    env.fork_copy("w");
    let view = env.view("w");
    edit(&view);
    let expected = fs::read(view.join("src/big.txt")).unwrap();
    env.ok(&["checkpoint", "w", "-m", "before pull"]);
    let id = latest_id(&env, "w");

    write(&env.project().join("other.txt"), "changed in the project\n");
    env.ok(&["pull", "w", "--no-install"]);
    fs::write(view.join("src/big.txt"), "later\n").unwrap();
    env.ok(&["restore", "w", &id.to_string(), "--no-install"]);
    assert_eq!(fs::read(view.join("src/big.txt")).unwrap(), expected);
}

#[test]
#[serial]
fn references_no_checkpoint_uses_are_removed() {
    let env = Env::new();
    write(&env.project().join("src/big.txt"), &source_text());
    env.fork_copy("w");
    edit(&env.view("w"));
    env.ok(&["checkpoint", "w"]);
    let refs = env.home.path().join("checkpoints/w/refs");
    let unused = refs.join("g99/forgotten.txt.zst");
    fs::create_dir_all(unused.parent().unwrap()).unwrap();
    fs::write(&unused, b"unused").unwrap();
    env.ok(&["checkpoint", "w"]);

    let used: Vec<PathBuf> = {
        let mut used: Vec<PathBuf> = fs::read_dir(env.home.path().join("checkpoints/w"))
            .unwrap()
            .flatten()
            .filter_map(|entry| fs::read(entry.path().join("record.json")).ok())
            .flat_map(|bytes| {
                let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                record["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|entry| {
                        entry["stored"]["reference"]
                            .as_str()
                            .map(|key| refs.join(key))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        used.sort();
        used.dedup();
        used
    };
    assert!(!used.is_empty());
    assert_eq!(walk(&refs), used);
    let _ = stdout;
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}
