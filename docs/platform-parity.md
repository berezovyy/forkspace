# Platform and backend parity

| Backend | Fork cost | New disk at fork | Diff mechanism | Conformance |
|---|---:|---:|---|---|
| Linux overlay (`linux`) | O(1) mount | approximately zero | upper-layer walk | Linux FUSE fixture |
| APFS clone (`mac-clone`) | one recursive clone | approximately zero | manifest + ctime scan | native macOS suite |
| Linux reflink (`linux-reflink`) | O(entries) | approximately zero, except per-file fallbacks | manifest + ctime scan | ioctl probe + parity suite |
| Portable copy (`copy`) | O(bytes) | full tree size | manifest + ctime scan | every-host parity suite |

| Feature | Linux overlay | APFS clone | Linux reflink | Copy | Evidence / disposition |
|---|---|---|---|---|---|
| Fork | instant mount | O(entries), CoW | O(entries), CoW per file | O(bytes) | `backend_probe`, native suite |
| Stacking | live parent view | sealed snapshot | sealed snapshot | sealed snapshot | `diff_parity::stacking_semantics_are_a_backend_property` |
| Diff / merge | upper walk / planned apply | manifest scan / planned apply | same | same | `diff_parity`, `merge_parity`, `backend_probe` |
| Pull | yes | yes | yes | yes | `tests/pull.rs` |
| Branch / land | yes | yes | yes | yes | `tests/land.rs` |
| Compare / signals | yes | yes | yes | yes | `tests/review_gate.rs` |
| Dev-server watcher | native host watcher | FSEvents | native host watcher | native host watcher | macOS runtime test; Linux fixtures |
| tmpfs / namespaces | backend capability only | non-goal | tmpfs non-goal | non-goal | capability contract |
| Crash reconciliation | yes | yes | yes | yes | `tests/fault_injection.rs` |

Selection is probed for each new fork and recorded permanently. Existing workspaces never
change backend because a faster capability is installed later. The copy backend is always
compiled in and is the portability floor; output and error contracts above the materializing
primitive remain identical.
