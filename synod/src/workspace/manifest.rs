//! What a folder holds at one moment: paths, kinds, sizes, timestamps.
//!
//! Nothing here ever opens a file.  A manifest is built from what
//! `symlink_metadata` yields and nothing else, which is what makes opening
//! a folder cost one stat-walk regardless of how many gigabytes are in it —
//! and what makes [`Change::Touched`](crate::workspace::changes::Change)
//! necessary, since a walk that never reads bytes cannot claim they differ.
//!
//! # On the filesystem calls below
//!
//! Building a manifest means walking the granted folder and stat-ing every
//! entry in it — [`Manifest::of_folder_via`]'s own walk, and the tests that
//! write fixture files for it to look at. This is synod's own before/after
//! bookkeeping, not the model's turn-time I/O, which runs inside the guest
//! and is gated there.
#![allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: the manifest walk stats the granted folder to record its \
              shape — synod's own bookkeeping, not the model's turn-time I/O. See the \
              module docs."
)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether `name` covers `path`: the same path, or a folder `path` sits
/// under.
///
/// The one folder-prefix-with-boundary rule the crate uses to resolve a
/// report's names — a file, or a folder and everything under it —
/// against the paths a manifest actually holds.
pub fn covers(path: &str, name: &str) -> bool {
    path == name
        || path
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// One entry in a [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File {
        size: u64,
        /// Nanoseconds since the Unix epoch, from the walk's own
        /// `symlink_metadata`; `0` on a platform or filesystem that yields
        /// none — honest for "unknown", and two entries both unknown
        /// compare equal, which is the quiet answer rather than a
        /// manufactured one.
        mtime_ns: u64,
        /// The file's Unix permission bits.  `0` on a non-Unix host, where
        /// no mode is ever read.
        mode: u32,
    },
    Folder,
    /// A symbolic link, recorded by its target text and never followed.
    Link {
        target: String,
    },
}

impl EntryKind {
    /// Whether two records describe the same thing, setting the timestamp
    /// aside.
    ///
    /// A timestamp moving is not by itself evidence that anything differs:
    /// a backup agent, a sync client, or a tool that rewrote a file with
    /// the bytes it already had moves it and changes nothing.  Since no
    /// manifest reads bytes any more, that case cannot be settled — so the
    /// diff tells it apart as
    /// [`Change::Touched`](crate::workspace::changes::Change::Touched)
    /// rather than folding it into an edit it cannot prove.
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::File { size, mode, .. },
                Self::File {
                    size: other_size,
                    mode: other_mode,
                    ..
                },
            ) => size == other_size && mode == other_mode,
            _ => self == other,
        }
    }
}

/// A folder's contents at one moment, keyed by `/`-joined relative path.
///
/// Folders are recorded too, so an empty one is a fact this can state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: BTreeMap<String, EntryKind>,
    /// Paths this walk listed and could not record, because they went
    /// away before it looked at them.  Not an error — a fact about what
    /// this manifest does and does not describe.
    #[serde(default)]
    pub unread: Vec<String>,
}

impl Manifest {
    /// Stat-walk `root` into a manifest.
    ///
    /// # Errors
    /// A plain sentence when something in the folder cannot be looked at,
    /// or when the folder itself is gone.
    pub fn of_folder(root: &Path) -> Result<Self, String> {
        Self::of_folder_via(root, &Stop::default(), &mut |_so_far| {})
    }

    /// Like [`Manifest::of_folder`], but observable and interruptible.
    ///
    /// `progress` is called after every file the walk records — with the
    /// running count, not the one file — so a caller walking a folder too
    /// big to finish in an eyeblink can show something other than a frozen
    /// window.  `stop` ends the walk at its next entry.
    ///
    /// # Errors
    /// A stopped walk is an error, never a short manifest: a truncated
    /// record of the folder would read as one where everything unwalked had
    /// been deleted.  A `root` that is gone is an error for the same reason,
    /// at the limit — an empty manifest would say the folder held nothing,
    /// and every file in the other manifest would answer to that as a
    /// deletion.  Only the root can raise `Vanished` this far: a subfolder
    /// that goes is recorded as unread and walked past.
    pub fn of_folder_via(root: &Path, stop: &Stop, progress: Progress<'_>) -> Result<Self, String> {
        let mut entries = BTreeMap::new();
        let mut unread = Vec::new();
        let mut files: u64 = 0;
        let mut visit =
            |key: &str, path: &Path, meta: &std::fs::Metadata| -> Result<(), WalkError> {
                if meta.file_type().is_symlink() {
                    let target = match std::fs::read_link(path) {
                        Ok(target) => target,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            return Err(WalkError::Vanished);
                        }
                        Err(e) => {
                            return Err(WalkError::Other(format!(
                                "Synod could not read the link {}: {e}.",
                                path.display()
                            )));
                        }
                    };
                    let Some(target) = target.to_str() else {
                        return Err(WalkError::Other(format!(
                            "The link {} points at a name this computer cannot read; \
                         please fix or remove it first.",
                            path.display()
                        )));
                    };
                    entries.insert(
                        key.to_string(),
                        EntryKind::Link {
                            target: target.to_string(),
                        },
                    );
                } else if meta.is_dir() {
                    entries.insert(key.to_string(), EntryKind::Folder);
                } else if meta.is_file() {
                    entries.insert(
                        key.to_string(),
                        EntryKind::File {
                            size: meta.len(),
                            mtime_ns: mtime_ns(meta),
                            mode: file_mode(meta),
                        },
                    );
                    files += 1;
                    progress(files);
                } else {
                    return Err(WalkError::Other(format!(
                        "{} is not an ordinary file, folder, or link, so synod could not \
                     account for it; please move it out of the folder first.",
                        path.display()
                    )));
                }
                Ok(())
            };
        match walk(root, "", &mut unread, stop, &mut visit) {
            Ok(()) => Ok(Self { entries, unread }),
            Err(WalkError::Vanished) => Err(format!(
                "Synod could not find {} any more — has the folder been moved, renamed, \
                 or unplugged?",
                root.display()
            )),
            Err(WalkError::Stopped) => Err("Synod stopped reading this folder before it had \
                                            finished, so it cannot say what changed."
                .to_string()),
            Err(WalkError::Other(message)) => Err(message),
        }
    }
}

/// A file's Unix permission bits; `0` on a host with no such notion.
#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0
}

/// A file's modification time, in nanoseconds since the Unix epoch; `0`
/// when the platform or filesystem yields none.
fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// How a walk being watched reports what it has found so far: the running
/// file count, not the one file that pushed it forward.
///
/// A borrowed `FnMut` rather than a generic, so [`Manifest::of_folder_via`]
/// stays a plain function callers can pass a closure or a no-op to without
/// fighting monomorphization.
pub type Progress<'a> = &'a mut dyn FnMut(u64);

/// A running walk's stop switch.
///
/// Shared with whoever may want the walk to end early: a conversation being
/// closed has no use for the baseline it started, and waiting that walk out
/// is not the same as stopping it.
#[derive(Clone, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    /// Ask the walk to stop at its next entry. Nothing resets this: a
    /// stopped walk is abandoned, never resumed.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// A vanished entry — a subfolder, or one the visitor finds already gone —
/// is not a real read failure: it tells [`walk`] to record `unread` and
/// move on, rather than fail the whole manifest.
enum WalkError {
    Vanished,
    Stopped,
    Other(String),
}

/// Record `key` as unread, unless a subtree root already recorded covers
/// it — the crate's one prefix rule, [`covers`].
fn record_unread(unread: &mut Vec<String>, key: String) {
    if !unread.iter().any(|root| covers(&key, root)) {
        unread.push(key);
    }
}

/// The two walks' unread paths, merged to the minimal covering set: a key
/// already covered by another survives only once.
pub(crate) fn merge_unread(a: &[String], b: &[String]) -> Vec<String> {
    let mut all: Vec<&String> = a.iter().chain(b).collect();
    all.sort();
    all.dedup();
    all.iter()
        .filter(|key| {
            !all.iter()
                .any(|other| *other != **key && covers(key, other))
        })
        .map(|key| (*key).clone())
        .collect()
}

/// How a live entry gets recorded.  `Err(Vanished)` tells [`walk`] this one
/// entry (not the whole subtree) disappeared underneath it; it folds into
/// `unread` exactly like a subfolder doing the same.
type Visit<'a> = &'a mut dyn FnMut(&str, &Path, &std::fs::Metadata) -> Result<(), WalkError>;

fn walk(
    dir: &Path,
    rel: &str,
    unread: &mut Vec<String>,
    stop: &Stop,
    visit: Visit<'_>,
) -> Result<(), WalkError> {
    let could_not = |e| {
        WalkError::Other(format!(
            "Synod could not look inside {}: {e}.",
            dir.display()
        ))
    };
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(WalkError::Vanished),
        Err(e) => return Err(could_not(e)),
    };
    for item in listing {
        if stop.stopped() {
            return Err(WalkError::Stopped);
        }
        let item = item.map_err(could_not)?;
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            return Err(WalkError::Other(format!(
                "A file in {} has a name this computer cannot read; please rename it first.",
                dir.display()
            )));
        };
        let key = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        let path = item.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // Listed, then gone before it could be looked at: unread, not
            // absent.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                record_unread(unread, key);
                continue;
            }
            Err(e) => {
                return Err(WalkError::Other(format!(
                    "Synod could not look at {}: {e}.",
                    path.display()
                )));
            }
        };
        if meta.is_dir() {
            match walk(&path, &key, unread, stop, visit) {
                Ok(()) => visit(&key, &path, &meta)?,
                Err(WalkError::Vanished) => record_unread(unread, key),
                Err(e) => return Err(e),
            }
        } else {
            match visit(&key, &path, &meta) {
                Ok(()) => {}
                Err(WalkError::Vanished) => record_unread(unread, key),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;

    #[test]
    fn files_folders_and_empty_folders_are_all_recorded() {
        let dir = workshop("manifest-record");
        std::fs::write(dir.path().join("letter.txt"), b"dear all").expect("fixture");
        std::fs::create_dir(dir.path().join("sent")).expect("fixture");
        std::fs::write(dir.path().join("sent").join("a.txt"), b"gone").expect("fixture");
        std::fs::create_dir(dir.path().join("empty")).expect("fixture");

        let manifest = Manifest::of_folder(dir.path()).expect("an ordinary folder reads");
        match manifest.entries.get("letter.txt") {
            Some(EntryKind::File { size, .. }) => assert_eq!(*size, 8),
            other => panic!("expected a recorded file, got {other:?}"),
        }
        assert_eq!(manifest.entries.get("sent"), Some(&EntryKind::Folder));
        assert!(manifest.entries.contains_key("sent/a.txt"));
        assert_eq!(
            manifest.entries.get("empty"),
            Some(&EntryKind::Folder),
            "an empty folder is a fact about the folder"
        );
    }

    /// A link is its target text — never followed, so a dangling one is
    /// fine and a link to a huge file costs nothing.
    #[cfg(unix)]
    #[test]
    fn a_link_is_recorded_as_its_target_and_never_followed() {
        let dir = workshop("manifest-link");
        std::os::unix::fs::symlink("nowhere/at-all", dir.path().join("dangling")).expect("fixture");
        let manifest =
            Manifest::of_folder(dir.path()).expect("a dangling link must not break the walk");
        assert_eq!(
            manifest.entries.get("dangling"),
            Some(&EntryKind::Link {
                target: "nowhere/at-all".into(),
            })
        );
    }

    #[test]
    fn the_walk_reports_progress_after_every_file() {
        let dir = workshop("manifest-progress");
        std::fs::write(dir.path().join("a.txt"), b"dear all").expect("fixture");
        std::fs::write(dir.path().join("b.txt"), b"gone").expect("fixture");
        std::fs::create_dir(dir.path().join("empty")).expect("fixture");

        let mut seen = Vec::new();
        Manifest::of_folder_via(dir.path(), &Stop::default(), &mut |so_far| {
            seen.push(so_far);
        })
        .expect("an ordinary folder reads");

        assert_eq!(
            seen,
            vec![1, 2],
            "one progress call per file — never per folder, never per byte"
        );
    }

    /// A stopped walk must not hand back the partial manifest as though it
    /// had finished: everything it never reached would read as deleted.
    #[test]
    fn a_walk_stopped_partway_errs_rather_than_answering_a_short_manifest() {
        let dir = workshop("manifest-stopped");
        std::fs::write(dir.path().join("a.txt"), b"dear all").expect("fixture");
        std::fs::write(dir.path().join("b.txt"), b"gone").expect("fixture");
        std::fs::write(dir.path().join("c.txt"), b"more").expect("fixture");

        let stop = Stop::default();
        let result = Manifest::of_folder_via(dir.path(), &stop, &mut |_so_far| stop.stop());

        assert!(
            result.is_err(),
            "a walk stopped after its first file must answer Err, never Ok(a short manifest)"
        );
    }

    #[test]
    fn record_unread_skips_a_key_already_covered_by_a_recorded_root() {
        let mut unread = vec!["target".to_string()];
        record_unread(&mut unread, "target/debug/build".to_string());
        assert_eq!(unread, vec!["target".to_string()]);

        record_unread(&mut unread, "vm-image".to_string());
        assert_eq!(unread, vec!["target".to_string(), "vm-image".to_string()]);
    }

    #[test]
    fn merge_unread_reduces_two_lists_to_their_minimal_covering_set() {
        let a = vec!["target".to_string(), "elsewhere.txt".to_string()];
        let b = vec!["target/debug".to_string(), "vm-image/out".to_string()];
        let mut merged = merge_unread(&a, &b);
        merged.sort();
        assert_eq!(
            merged,
            vec![
                "elsewhere.txt".to_string(),
                "target".to_string(),
                "vm-image/out".to_string(),
            ]
        );
    }
}
