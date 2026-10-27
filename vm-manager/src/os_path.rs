//! Lossless paths through serde's platform-native `OsString` encoding.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(crate) fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    path.as_os_str().serialize(serializer)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    OsString::deserialize(deserializer).map(PathBuf::from)
}

pub(crate) mod list {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        paths: &[PathBuf],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(paths.iter().map(|path| path.as_os_str()))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<OsString>::deserialize(deserializer)
            .map(|paths| paths.into_iter().map(PathBuf::from).collect())
    }
}

pub(crate) mod option {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(
        clippy::ref_option,
        reason = "serde's `with` borrows the declared field"
    )]
    pub(crate) fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_ref()
            .map(|path| path.as_os_str())
            .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Option::<OsString>::deserialize(deserializer).map(|path| path.map(PathBuf::from))
    }
}
