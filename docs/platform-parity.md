# Platform and backend parity

| Backend | Fork cost | New disk at fork | Diff mechanism | Conformance |
|---|---:|---:|---|---|
| Linux overlay (`linux`) | O(1) mount over a frozen copy: first fork of a project copies it (reflink or bytes), later forks reuse or hard-link it | first fork: one copy per project (zero on Btrfs/XFS) | upper-layer walk | `tests/diff_parity.rs::cross_platform_mutation_script_has_one_normalized_json_truth` (FUSE CI) |
| APFS clone (`mac-clone`) | one recursive clone | approximately zero | manifest + ctime scan | `tests/macos_clone.rs::fork_is_native_isolated_and_preserves_inherited_dependencies` |
| Linux reflink (`linux-reflink`) | O(entries) | approximately zero, except per-file fallbacks | manifest + ctime scan | `tests/backend_probe.rs::requested_real_reflink_volume_materializes_without_data_copy` |
| Portable copy (`copy`) | O(bytes) | full tree size | manifest + ctime scan | `tests/backend_probe.rs::plain_copy_is_a_privilege_free_fidelity_floor` |

| Feature | Linux overlay | APFS clone | Linux reflink | Copy | Evidence / disposition |
|---|---|---|---|---|---|
| Fork | instant mount | O(entries), CoW | O(entries), CoW per file | O(bytes) | `tests/backend_probe.rs::probe_report_is_ordered_and_uses_the_ioctl_verdict` |
| Stacking | sealed (frozen copy of the parent view) | sealed snapshot | sealed snapshot | sealed snapshot | `tests/diff_parity.rs::copy_stack_is_a_snapshot_and_merges_exactly_one_level_at_a_time` |
| Diff / merge | upper walk / planned apply | manifest scan / planned apply | same | same | `tests/merge_parity.rs::selected_directory_backend_round_trips_through_the_public_cli` |
| Pull | yes | yes | yes | yes | `tests/pull.rs::merge_after_pull_pushes_only_remaining_workspace_edits` |
| Branch / land | yes | yes | yes | yes | `tests/land.rs::public_fork_branch_then_land_preserves_the_workspace_commits` |
| Compare / signals | yes | yes | yes | yes | `tests/review_gate.rs::compare_preserves_argument_order_and_buckets_divergence` |
| Dev-server watcher | native host watcher | FSEvents | native host watcher | native host watcher | `tests/macos_clone.rs::run_switch_proxy_hot_reload_and_drop_supervision` |
| tmpfs / namespaces | `--tmpfs` keeps upper and work on an existing tmpfs | explicit non-goal | explicit non-goal | explicit non-goal | `tests/fork_drop.rs::test_fork_tmpfs_keeps_overlay_layers_in_ram` |
| `.wh.*` names | refused at fork; cannot be created in the view | ordinary files | ordinary files | ordinary files | `tests/fork_drop.rs::test_overlay_fork_refuses_base_with_whiteout_names` |
| Crash reconciliation | yes | yes | yes | yes | `tests/fault_injection.rs::kill_during_copy_fork_reconciles_the_intent_and_partial_tree` |

Selection is probed for each new fork and recorded permanently. Existing workspaces never
change backend because a faster capability is installed later. The copy backend is always
compiled in and is the portability floor; output and error contracts above the materializing
primitive remain identical.
