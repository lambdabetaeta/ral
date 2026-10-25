//! Filesystem query records and their pure construction from observed metadata.

use crate::path::EntryName;
use crate::path::shown;
use crate::path::walk::{Kind, Stat};
use crate::record;
use std::ffi::OsStr;
use std::time::SystemTime;

/// Seconds since the epoch, or 0 when the field is unrecorded or pre-epoch.
fn secs_since_epoch(t: Option<SystemTime>) -> i64 {
    t.map_or(0, super::unix_secs)
}

/// What `list-dir` reports of one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirEntry {
    name: String,
    kind: Kind,
    size: u64,
    mtime: i64,
}

record!(typed DirEntry {
    name: "name",
    kind: "type",
    size: "size",
    mtime: "mtime",
});

impl DirEntry {
    pub(crate) fn of(name: &EntryName, stat: &Stat) -> Self {
        let Stat {
            kind, len, mtime, ..
        } = *stat;
        Self {
            name: shown(name.as_os_str()),
            kind,
            size: len,
            mtime: secs_since_epoch(mtime),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

/// What `file-info` reports of one path: a [`DirEntry`]'s fields, access and
/// birth times, the readonly bit, and the symlink `target` when it is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileInfo {
    name: String,
    kind: Kind,
    size: u64,
    mtime: i64,
    atime: i64,
    btime: i64,
    readonly: bool,
    target: Option<String>,
}

record!(typed FileInfo {
    name: "name",
    kind: "type",
    size: "size",
    mtime: "mtime",
    atime: "atime",
    btime: "btime",
    readonly: "readonly",
    target: "target",
});

impl FileInfo {
    pub(crate) fn of(name: &OsStr, stat: &Stat, target: Option<String>) -> Self {
        Self {
            name: shown(name),
            kind: stat.kind,
            size: stat.len,
            mtime: secs_since_epoch(stat.mtime),
            atime: secs_since_epoch(stat.atime),
            btime: secs_since_epoch(stat.btime),
            readonly: stat.readonly,
            target,
        }
    }
}
