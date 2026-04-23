# Platform and backend parity

| Backend | Fork cost | New disk at fork | Diff mechanism | Conformance |
|---|---:|---:|---|---|
| Linux overlay (`linux`) | O(1) mount | approximately zero | upper-layer walk | Linux FUSE fixture |
| APFS clone (`mac-clone`) | one recursive clone | approximately zero | manifest + ctime scan | native macOS suite |
| Linux reflink (`linux-reflink`) | O(entries) | approximately zero, except per-file fallbacks | manifest + ctime scan | ioctl probe + parity suite |
| Portable copy (`copy`) | O(bytes) | full tree size | manifest + ctime scan | every-host parity suite |

Selection is probed for each new fork and recorded permanently. Existing workspaces never
change backend because a faster capability is installed later. The copy backend is always
compiled in and is the portability floor; output and error contracts above the materializing
primitive remain identical.
