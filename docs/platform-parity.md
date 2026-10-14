# Platforms and backends

The commands and their output are the same everywhere; what differs is how a workspace is made
and what the OS offers around it. Each row names the test that holds it. Measured times are in
[benchmarks.md](benchmarks.md).

## Backends

| Backend | Fork cost | New disk at fork | Diff mechanism | Conformance |
|---|---:|---:|---|---|
| Linux overlay (`linux`) | O(1) mount over a frozen copy: first fork of a project copies it (reflink or bytes), later forks reuse or hard-link it | first fork: one copy per project (zero on Btrfs/XFS) | upper-layer walk | `tests/diff_parity.rs::cross_platform_mutation_script_has_one_normalized_json_truth` (FUSE CI) |
| APFS clone (`mac-clone`) | one recursive clone | approximately zero | manifest + ctime scan | `tests/macos_clone.rs::fork_is_native_isolated_and_preserves_inherited_dependencies` |
| Linux reflink (`linux-reflink`) | O(entries) | approximately zero, except per-file fallbacks | manifest + ctime scan | `tests/backend_probe.rs::requested_real_reflink_volume_materializes_without_data_copy` |
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
| Checkpoints | yes | yes | yes | yes | `tests/checkpoints.rs::checkpoint_then_restore_is_exact_on_the_copy_backend` |
| Branch / land | yes | yes | yes | yes | `tests/land.rs::public_fork_branch_then_land_preserves_the_workspace_commits` |
| Compare / signals | yes | yes | yes | yes | `tests/review_gate.rs::compare_preserves_argument_order_and_buckets_divergence` |
| Dev-server watcher | native host watcher | FSEvents | native host watcher | native host watcher | `tests/macos_clone.rs::run_switch_proxy_hot_reload_and_drop_supervision` |
| `--tmpfs` | changes on an existing tmpfs | refused | refused | refused | `tests/fork_drop.rs::test_fork_tmpfs_keeps_overlay_layers_in_ram` |
| `.wh.*` names | refused at fork; cannot be created in the view | ordinary files | ordinary files | ordinary files | `tests/fork_drop.rs::test_overlay_fork_refuses_base_with_whiteout_names` |
| Crash reconciliation | yes | yes | yes | yes | `tests/fault_injection.rs::kill_during_copy_fork_reconciles_the_intent_and_partial_tree` |

Selection is probed for each new fork and recorded permanently. Existing workspaces never
change backend because a faster capability is installed later. The copy backend is always
compiled in, and everything above the primitive that materializes a workspace is shared.

## Around the workspace

| Feature | Linux | macOS | Evidence |
|---|---|---|---|
| Processes in a workspace | systemd user slice, else a `/proc` scan | libproc scan | `tests/processes.rs::processes_that_leave_the_workspace_directory_are_still_killed_through_the_slice`; `src/process/members.rs::libproc_finds_a_process_working_in_the_view` |
| Limits | `MemoryHigh`, `CPUQuota`, one shared `forkspace.slice` | background priority | `tests/processes.rs::workspace_processes_run_under_forkspace_slice_with_limits`; `src/process/systemd.rs::background_commands_run_at_background_priority` |
| Write sandbox | Landlock | `sandbox-exec` (Seatbelt) | `tests/sandbox.rs::writes_to_the_project_are_refused_and_writes_inside_succeed`; `tests/sandbox_macos.rs::seatbelt_refuses_writes_to_the_project_and_allows_the_view` |
| Disk use | allocated blocks, stopping at mount points | APFS private size | `tests/disk.rs::ls_shows_private_sizes_and_a_total`; `src/disk.rs::apfs_clones_have_no_private_size` |
| Directory scans | parallel `readdir` + `lstat` | `getattrlistbulk`, checked against `lstat` | `tests/scans.rs::merge_ls_and_diff_agree_while_node_modules_is_skipped`; `src/util/bulk.rs::bulk_attributes_match_lstat` |

The macOS tests in this table compile in CI but have not yet run on a Mac; the
[roadmap](roadmap.md) tracks that run.

## Limitations

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
