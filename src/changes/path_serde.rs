//! Serde for change paths that keeps non-UTF-8 paths lossless.
//!
//! UTF-8 paths serialize as plain strings. Any other path becomes `"\0forkspace-bytes:"` followed
//! by its bytes in lowercase hex; the NUL prefix cannot occur in a real path.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};

const BYTE_PREFIX: &str = "\0forkspace-bytes:";

pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    if let Some(path) = path.to_str() {
        return serializer.serialize_str(path);
    }
    let mut encoded = String::from(BYTE_PREFIX);
    for byte in path.as_os_str().as_bytes() {
        write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    serializer.serialize_str(&encoded)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    let encoded = String::deserialize(deserializer)?;
    let Some(hex) = encoded.strip_prefix(BYTE_PREFIX) else {
        return Ok(PathBuf::from(encoded));
    };
    if hex.len() % 2 != 0 {
        return Err(D::Error::custom("invalid encoded path length"));
    }
    let bytes = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| D::Error::custom("invalid encoded path byte"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}
