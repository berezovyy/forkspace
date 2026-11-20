# Platforms and backends

The commands and their output are the same everywhere; what differs is how a workspace is made
and what the OS offers around it. Each row names the test that holds it. Measured times are in
[benchmarks.md](benchmarks.md).

## Backends

| Backend | Fork cost | New disk at fork | Diff mechanism | Conformance |
|---|---:|---:|---|---|
| Linux overlay (`linux`) | O(1) mount over a frozen copy: first fork of a project copies it (reflink or bytes), later forks reuse or hard-link it | first fork: one copy per project (zero on Btrfs/XFS) | upper-layer walk | `tests/diff_parity.rs::cross_platform_mutation_script_has_one_normalized_json_truth` (FUSE CI) |
| APFS clone (`mac-clone`) | one recursive clone | approximately zero | manifest + ctime scan | `tests/macos_clone.rs::fork_is_native_isolated_and_preserves_inherited_dependencies` |
| Linux reflink (`linux-reflink`) | O(entries) | approximately zero, except per-file fallbacks | manifest + ctime scan | `tests/backend_probe.rs::a_real_reflink_volume_materializes_without_data_copy` |
| Portable copy (`copy`) | O(bytes) | full tree size | manifest + ctime scan | `tests/backend_probe.rs::plain_copy_is_a_privilege_free_fidelity_floor` |

| Feature | Linux overlay | APFS clone | Linux reflink | Copy | Evidence |
|---|---|---|---|---|---|
| Fork | instant mount | O(entries), CoW | O(entries), CoW per file | O(bytes) | `tests/backend_probe.rs::probe_report_is_ordered_and_uses_the_ioctl_verdict` |
| Sealed against base changes | frozen copy | by construction | by construction | by construction | `tests/sealed.rs::branch_switches_and_commits_in_the_base_do_not_reach_the_workspace` |
| Stacking | frozen copy of the parent view | snapshot | snapshot | snapshot | `tests/diff_parity.rs::copy_stack_is_a_snapshot_and_merges_exactly_one_level_at_a_time` |
| Diff / merge | upper walk / planned apply | manifest scan / planned apply | same | same | `tests/merge_parity.rs::selected_directory_backend_round_trips_through_the_public_cli` |
| Line-level merge | yes | yes | yes | yes | `tests/line_merge.rs::different_lines_reach_the_project_from_both_workspaces` |
| Merge undo | yes | yes | yes | yes | `tests/merge_undo.rs::undo_restores_the_exact_project_and_removes_what_merge_added` |
| Pull | yes | yes | yes | yes | `tests/pull.rs::merge_after_pull_pushes_only_remaining_workspace_edits` |
| Checkpoints | yes | yes | yes | yes | `tests/checkpoints.rs::checkpoint_then_restore_is_exact` |
| Commits kept as a branch on merge | yes | yes | yes | yes | `tests/merge_commits.rs::commits_are_kept_as_a_branch_and_the_project_branch_is_left_alone` |
| Compare / signals | yes | yes | yes | yes | `tests/review_gate.rs::compare_preserves_argument_order_and_buckets_divergence` |
| Dev-server watcher | native host watcher | FSEvents | native host watcher | native host watcher | `tests/macos_clone.rs::run_switch_proxy_hot_reload_and_drop_supervision` |
| `.wh.*` names | refused at fork; cannot be created in the view | ordinary files | ordinary files | ordinary files | `tests/fork_drop.rs::a_base_with_whiteout_names_is_copied_instead_of_layered` |
| Crash reconciliation | yes | yes | yes | yes | `tests/fault_injection.rs::kill_during_copy_fork_reconciles_the_intent_and_partial_tree` |

Selection is probed for each new fork and recorded permanently. Existing workspaces never
change backend because a faster capability is installed later. The copy backend is always
compiled in, and everything above the primitive that materializes a workspace is shared.

## Around the workspace

| Feature | Linux | macOS | Evidence |
|---|---|---|---|
| Processes in a workspace | systemd user slice, else a `/proc` scan | libproc scan | `tests/processes.rs::processes_that_leave_the_workspace_directory_are_still_killed_through_the_slice`; `src/process/members.rs::libproc_finds_a_process_working_in_the_view` |
| Limits | one shared `forkspace.slice` (CPU weight 50, 75% memory ceiling) | background priority | `tests/processes.rs::workspace_processes_run_under_the_shared_forkspace_slice`; `src/process/systemd.rs::background_commands_run_at_background_priority` |
| Write sandbox | Landlock | `sandbox-exec` (Seatbelt) | `tests/sandbox.rs::writes_to_the_project_are_refused_and_writes_inside_succeed`; `tests/sandbox_macos.rs::seatbelt_refuses_writes_to_the_project_and_allows_the_view` |
| Disk use | allocated blocks, stopping at mount points | APFS private size | `tests/disk.rs::ls_shows_private_sizes_and_a_total`; `src/disk.rs::apfs_clones_have_no_private_size` |
| Directory scans | parallel `readdir` + `lstat` | `getattrlistbulk`, checked against `lstat` | `tests/scans.rs::merge_ls_and_diff_agree_while_node_modules_is_skipped`; `src/util/bulk.rs::bulk_attributes_match_lstat` |

The macOS tests in this table run on Apple Silicon (macOS 26, APFS). Tests that drive Linux
overlay layers directly are skipped there; the overlay backend itself is Linux-only.

## Many workspaces on Linux

What each overlay workspace costs, and the limits that apply first, measured on a Linux 7.0
desktop (ext4 on NVMe, fuse-overlayfs 1.13, 16 CPUs, 31 GiB, no swap; 2026-11-19):

| Resource | Per overlay workspace | Limit or effect |
|---|---|---|
| fuse-overlayfs daemon | about 1.4 MB resident | none in practice: 1,000 workspaces ≈ 1.4 GB |
| FUSE mounts | one | `fusermount` allows 1,000 mounts in total unless root raises `mount_max` in `/etc/fuse.conf` |
| Page cache for files read through the view | one copy per workspace, on top of the frozen copy's own | reading one 300 MB file through 3 workspaces cached 1.2 GB; the cache is reclaimable, but the copies compete for it |
| Disk for unchanged files | none: every workspace of a project reads the same frozen copy | the first fork of a project copies it once (bytes on ext4, clones on Btrfs/XFS) |
| `fsync` | answered without flushing (`fsync=0`) | 1,000 small write+fsync calls in 80 ms instead of 1,390 ms; `merge` still flushes what it writes into the project |
| Git index | refreshed once in the frozen copy, not written per workspace | first `git status`: no hashing; about 300 ms of cold FUSE lookups for 20,000 files |
| `state.json` | about 700 bytes | at 1,000 workspaces: 700 KB, `diff` 14 ms, `new` 40–80 ms; every command reads it whole under one lock, so a `diff` waited up to 620 ms while 30 `new` ran at once |
| inotify | whatever watchers the agent runs | 256 inotify instances per user by default (`fs.inotify.max_user_instances`): one per watching process, the first limit file watchers reach |

Kernel overlayfs would share one page cache between workspaces, but mounting it without root
needs an unprivileged user namespace, which distributions such as Ubuntu 24.04+ restrict
(`kernel.apparmor_restrict_unprivileged_userns=1`). Without swap, the memory of idle agent
processes cannot be reclaimed, and merging identical pages (KSM) needs root to enable.

## Limitations

- Forks leave out Git worktrees nested in the project, `.claude/worktrees/` and what
  `.forkspaceexclude` lists (`tests/exclusions.rs`).
- On the overlay backend, the first `git status` in a new workspace still costs one `lstat`
  per tracked file through FUSE (about 300 ms for 20,000 files cold); the index itself is
  already fresh (`tests/index_refresh.rs`).
- APFS `clonefile` materializes directory metadata, so fork cost grows with the number of
  entries even though file blocks are shared.
- APFS cloning needs the project and `FORKSPACE_HOME/workspaces` on the same APFS volume;
  `--copy` is the explicit way out.
- Snapshot backends find changes with the fork manifest plus ctime. A tool that changes bytes
  and then restores every observable piece of metadata is outside the contract.
- Caches outside the project, such as `~/.npm` and `~/.cargo`, are shared between workspaces,
  and postinstall scripts can write outside the project.
- Build caches keyed by absolute path may rebuild, because the workspace lives at another path.
- Deep stacks of large snapshot trees multiply metadata and scan cost; stacks stop at 32
  levels.
- `land` carries superproject commits, but not Git LFS objects or commits inside a submodule's
  own repository.
