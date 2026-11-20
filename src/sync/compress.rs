//! zstd for checkpoint files, against the fork-time version when there is one.
//!
//! An edited file usually differs from its fork-time version in a few lines, so it is stored
//! as a zstd frame that uses that version as a reference prefix (what `zstd --patch-from`
//! does): a 300 KB source file with a few edited lines takes a few hundred bytes instead of
//! another 300 KB. Frames carry a checksum of their content, so decoding against the wrong
//! reference fails instead of restoring something else.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use crate::error::{ForkspaceError, Result};
use crate::util::IoContext;

/// The level files are compressed at: fast, and checkpoints are mostly text.
const LEVEL: i32 = 3;
/// References larger than this are not used: decoding holds the whole reference in memory.
pub const MAX_REFERENCE: u64 = 64 << 20;
/// Files larger than this are copied, not compressed.
pub const MAX_COMPRESSED: u64 = 256 << 20;

/// The window, as a power of two, that lets a frame reach back over all of `reference` and
/// `len` bytes of content.
fn window_log(reference: usize, len: u64) -> u32 {
    let span = reference as u64 + len + 1;
    (64 - span.leading_zeros()).clamp(10, 27)
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> ForkspaceError {
    let context = context.into();
    move |source| ForkspaceError::Io { context, source }
}

/// Compress `source` into `target`, against `reference` when given. Returns the size of
/// `target`.
pub fn encode(source: &Path, reference: Option<&[u8]>, target: &Path) -> Result<u64> {
    let len = fs::metadata(source)
        .io_context(|| format!("inspect {}", source.display()))?
        .len();
    let mut input = fs::File::open(source).io_context(|| format!("open {}", source.display()))?;
    let output = fs::File::create(target).io_context(|| format!("create {}", target.display()))?;
    let context = format!("compress {}", source.display());
    let mut encoder = match reference {
        Some(reference) => {
            let mut encoder =
                zstd::stream::write::Encoder::with_ref_prefix(output, LEVEL, reference)
                    .map_err(io(context.clone()))?;
            encoder
                .window_log(window_log(reference.len(), len))
                .and_then(|()| encoder.long_distance_matching(true))
                .map_err(io(context.clone()))?;
            encoder
        }
        None => zstd::stream::write::Encoder::new(output, LEVEL).map_err(io(context.clone()))?,
    };
    encoder
        .include_checksum(true)
        .and_then(|()| encoder.set_pledged_src_size(Some(len)))
        .map_err(io(context.clone()))?;
    std::io::copy(&mut input, &mut encoder).map_err(io(context.clone()))?;
    let output = encoder.finish().map_err(io(context.clone()))?;
    output.sync_all().map_err(io(context))?;
    Ok(fs::metadata(target)
        .io_context(|| format!("inspect {}", target.display()))?
        .len())
}

/// Decompress `stored` into `target`, against the same `reference` it was compressed with.
pub fn decode(stored: &Path, reference: Option<&[u8]>, target: &Path) -> Result {
    let input = fs::File::open(stored).io_context(|| format!("open {}", stored.display()))?;
    let mut output =
        fs::File::create(target).io_context(|| format!("create {}", target.display()))?;
    let context = format!("decompress {}", stored.display());
    let input = std::io::BufReader::new(input);
    let mut decoder: Box<dyn Read> = match reference {
        Some(reference) => {
            let mut decoder = zstd::stream::read::Decoder::with_ref_prefix(input, reference)
                .map_err(io(context.clone()))?;
            decoder.window_log_max(31).map_err(io(context.clone()))?;
            Box::new(decoder)
        }
        None => {
            let mut decoder =
                zstd::stream::read::Decoder::with_buffer(input).map_err(io(context.clone()))?;
            decoder.window_log_max(31).map_err(io(context.clone()))?;
            Box::new(decoder)
        }
    };
    std::io::copy(&mut decoder, &mut output).map_err(io(context))?;
    output
        .flush()
        .io_context(|| format!("write {}", target.display()))
}

/// Compress `bytes` into `target` (references are kept compressed too).
pub fn encode_bytes(bytes: &[u8], target: &Path) -> Result {
    let compressed =
        zstd::bulk::compress(bytes, LEVEL).map_err(io(format!("compress {}", target.display())))?;
    fs::write(target, compressed).io_context(|| format!("write {}", target.display()))
}

/// The bytes [`encode_bytes`] stored at `path`.
pub fn decode_bytes(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).io_context(|| format!("open {}", path.display()))?;
    zstd::stream::decode_all(file).map_err(io(format!("decompress {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_edit_of_a_large_file_stores_a_few_hundred_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let original: String = (0..6000)
            .map(|line| {
                format!(
                    "line {line}: some source text that repeats a little {}\n",
                    line * 7
                )
            })
            .collect();
        let mut edited = original.clone();
        edited = edited.replacen("line 3000:", "line 3000 (edited):", 1);
        edited.push_str("fn added() {}\n");
        fs::write(dir.path().join("edited"), &edited).unwrap();

        let stored = dir.path().join("stored");
        let size = encode(
            &dir.path().join("edited"),
            Some(original.as_bytes()),
            &stored,
        )
        .unwrap();
        assert!(
            size < 1_000,
            "{size} bytes for a {}-byte file",
            edited.len()
        );
        decode(&stored, Some(original.as_bytes()), &dir.path().join("back")).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("back")).unwrap(), edited);

        // The wrong reference is an error, never other content.
        let wrong = original.replace("some", "SOME");
        assert!(decode(&stored, Some(wrong.as_bytes()), &dir.path().join("bad")).is_err());

        let plain = dir.path().join("plain");
        encode(&dir.path().join("edited"), None, &plain).unwrap();
        decode(&plain, None, &dir.path().join("plain-back")).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("plain-back")).unwrap(),
            edited
        );
    }
}
