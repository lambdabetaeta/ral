//! A path as data that crosses a process boundary: the operating system's own
//! units, every one of them.
//!
//! serde's own `PathBuf` goes through `str`, so a path that is not Unicode — a
//! Unix name in some older byte encoding, a Windows name holding an unpaired
//! surrogate — does not serialise at all.  On the broker's pipe that is the
//! worst way to fail: the frame carrying a refusal is never written, and the
//! error that was to explain why a folder could not be granted vanishes with
//! it.  So every path in this crate's wire types travels as the units its
//! platform names it by — bytes on Unix, UTF-16 code units on Windows, those
//! little-endian — and is rebuilt from exactly those on the far side.
//!
//! The units travel as one base64 string.  JSON, the broker's framing, has no
//! byte type: `serde_json` writes a `Vec<u8>` or `Vec<u16>` as an array of
//! decimal numbers, up to six bytes on the wire for every UTF-16 unit, where
//! base64 costs a uniform 8/3 per unit — 87,380 bytes for the longest path
//! Windows has, 32,767 units.  The broker's frame limit is sized from that
//! figure (`broker::frame::MAX`).
//!
//! The units need no translation, because both ends of every pipe this crate
//! speaks run on the one platform: the broker is Windows on both sides.
//!
//! Used as `#[serde(with = "crate::os_path")]`, and [`option`] for an
//! `Option<PathBuf>`.

use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serializer, de};

/// The path's units, as the bytes base64 encodes: lossless, so two paths
/// encode alike only if they are the one path.
#[cfg(unix)]
pub(crate) fn encode(path: &Path) -> String {
    STANDARD.encode(path.as_os_str().as_bytes())
}

/// The path's units, as the bytes base64 encodes: lossless, so two paths
/// encode alike only if they are the one path.
#[cfg(windows)]
pub(crate) fn encode(path: &Path) -> String {
    let bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    STANDARD.encode(bytes)
}

/// The path whose units `text` encodes.
///
/// # Errors
/// `E`'s own, if `text` is not base64.
#[cfg(unix)]
fn decode<E: de::Error>(text: &str) -> Result<PathBuf, E> {
    let bytes = STANDARD.decode(text).map_err(E::custom)?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// The path whose units `text` encodes.
///
/// # Errors
/// `E`'s own, if `text` is not base64 or does not decode to whole UTF-16
/// units.
#[cfg(windows)]
fn decode<E: de::Error>(text: &str) -> Result<PathBuf, E> {
    let bytes = STANDARD.decode(text).map_err(E::custom)?;
    let (pairs, []) = bytes.as_chunks::<2>() else {
        return Err(E::custom(
            "a path's UTF-16 units arrived as an odd number of bytes",
        ));
    };
    let units: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

/// # Errors
/// Only the serializer's own.
pub(crate) fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&encode(path))
}

/// # Errors
/// The deserializer's own, if what arrived is not a path's units in base64.
pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    decode(&String::deserialize(deserializer)?)
}

/// The same encoding, for a path that may be absent.
pub(crate) mod option {
    use std::path::PathBuf;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{decode, encode};

    /// # Errors
    /// Only the serializer's own.
    #[allow(
        clippy::ref_option,
        reason = "serde's `with` hands the field over by reference, as it is declared"
    )]
    pub(crate) fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_deref().map(encode).serialize(serializer)
    }

    /// # Errors
    /// The deserializer's own, if what arrived is neither nothing nor a path's
    /// units in base64.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .as_deref()
            .map(decode)
            .transpose()
    }
}
