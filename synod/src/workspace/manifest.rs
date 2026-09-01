//! What a folder holds at one moment: paths, kinds, sizes, content hashes.

use crate::workspace::restore::covers;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Warn before checkpointing a folder bigger than this.
///
/// A checkpoint reads every byte, before and after each job.  2 GiB keeps
/// that in the seconds on a local disk; past it — especially on a
/// departmental share at tens of MB/s — the wait reaches minutes and the
/// user deserves a heads-up before it starts.
pub const LARGE_FOLDER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How a walk turns a file into its size and hash — the history store hooks
/// in here to keep the bytes while it hashes them, or to reuse a hash
/// already on record when the stat facts say the bytes cannot have moved.
///
/// Given the entry's `/`-joined key, its path, and the size and mtime the
/// walk already read from `symlink_metadata`, it answers the size and hash
/// to record — or `None` when the file vanished between the walk's listing
/// and the hook's own read of it, in which case the walk records nothing.
pub(crate) type FileEntry<'a> =
    &'a mut dyn FnMut(&str, &Path, u64, u64) -> Result<Option<(u64, ContentHash)>, String>;

/// A blake3 hash of a file's bytes, hex-encoded.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<blake3::Hash> for ContentHash {
    fn from(hash: blake3::Hash) -> Self {
        Self(hash.to_hex().to_string())
    }
}

/// One entry in a [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File {
        size: u64,
        hash: ContentHash,
        /// The file's Unix permission bits, so a restore puts back the
        /// same mode it recorded — not the object store's own default.
        /// `0` on a non-Unix host, where no mode is ever read or applied.
        mode: u32,
        /// Nanoseconds since the Unix epoch, from the walk's own
        /// `symlink_metadata`; `0` on a platform that yields none, or for
        /// a record written before this field existed — either way, honest
        /// for "unknown, always re-read".
        #[serde(default)]
        mtime_ns: u64,
    },
    Folder,
    /// A symbolic link, recorded by its target text and never followed.
    Link {
        target: String,
    },
}

/// A folder's contents at one moment, keyed by `/`-joined relative path.
///
/// Folders are recorded too, so empty ones survive a round trip.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: BTreeMap<String, EntryKind>,
    /// Paths this walk listed and could not record, because they went
    /// away before it read them.  Not an error — a fact about what this
    /// manifest does and does not describe.
    #[serde(default)]
    pub unread: Vec<String>,
}

impl Manifest {
    /// Read `root` into a manifest, hashing every file.
    ///
    /// # Errors
    /// A plain sentence when something in the folder cannot be read, or when
    /// the folder itself is gone.
    pub fn of_folder(root: &Path) -> Result<Self, String> {
        Self::of_folder_via(
            root,
            &Stop::default(),
            &mut |_key, path, _size, _mtime_ns| hash_file(path),
        )
    }

    /// Like [`Manifest::of_folder`], but `file_entry` decides how each
    /// file's size and hash are produced, and `stop` can end the walk part
    /// way through.
    ///
    /// # Errors
    /// A stopped walk is an error, never a short manifest: a truncated
    /// record of the folder would read as one where everything unread had
    /// been deleted.  A `root` that is gone is an error for the same reason,
    /// at the limit — an empty manifest would say the folder held nothing,
    /// and every file in the other manifest would answer to that as a
    /// creation to undo.  Only the root can raise `Vanished` this far: a
    /// subfolder that goes is recorded as unread and walked past.
    pub(crate) fn of_folder_via(
        root: &Path,
        stop: &Stop,
        file_entry: FileEntry<'_>,
    ) -> Result<Self, String> {
        let mut entries = BTreeMap::new();
        let mut unread = Vec::new();
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
                    let size = meta.len();
                    let file_mtime_ns = mtime_ns(meta);
                    match file_entry(key, path, size, file_mtime_ns).map_err(WalkError::Other)? {
                        Some((size, hash)) => {
                            entries.insert(
                                key.to_string(),
                                EntryKind::File {
                                    size,
                                    hash,
                                    mode: file_mode(meta),
                                    mtime_ns: file_mtime_ns,
                                },
                            );
                        }
                        None => return Err(WalkError::Vanished),
                    }
                } else {
                    return Err(WalkError::Other(format!(
                        "{} is not an ordinary file, folder, or link, so synod could not \
                     promise to put it back; please move it out of the folder first.",
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

/// Hash a file's bytes, streaming.
///
/// # Errors
/// A plain sentence when the file cannot be read for a reason other than
/// having vanished — a vanished file answers `Ok(None)`.
pub(crate) fn hash_file(path: &Path) -> Result<Option<(u64, ContentHash)>, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Synod could not read {}: {e}.", path.display())),
    };
    let mut hasher = blake3::Hasher::new();
    let size = std::io::copy(&mut file, &mut hasher)
        .map_err(|e| format!("Synod could not read {}: {e}.", path.display()))?;
    Ok(Some((size, ContentHash::from(hasher.finalize()))))
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

/// A running walk's stop switch, shared with whoever may want it to end
/// early — a conversation being ended has no use for the copy it started,
/// and waiting that copy out is not the same as stopping it.
#[derive(Clone, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    /// Ask the walk to stop at its next entry. Nothing resets this: a
    /// stopped copy is abandoned, never resumed.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// A vanished entry — a subfolder, or one a visitor finds already gone —
/// is not a real read failure: it tells [`walk`] to record `unread` and
/// move on, rather than fail the whole capture.
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

/// How a live entry gets recorded — `of_folder_via`'s visitor builds a
/// [`Manifest`]; `measure`'s counts bytes.  `Err(Vanished)` tells [`walk`]
/// this one entry (not the whole subtree) disappeared underneath it; it
/// folds into `unread` exactly like a subfolder doing the same.
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
            // absent, the same as a file gone before it could be read.
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

/// How much a folder holds, without reading a single file's bytes — the
/// large-folder warning's pre-walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Measure {
    pub files: u64,
    pub bytes: u64,
}

/// Stat-walk `root` to count its regular files and sum their sizes.
///
/// Symlinks and folders cost nothing; a path that vanishes mid-walk is
/// simply not counted, the same as a capture would treat it.
///
/// # Errors
/// A plain sentence when the folder cannot be looked inside.
pub fn measure(root: &Path) -> Result<Measure, String> {
    let mut measure = Measure::default();
    // Symlinks and folders never reach the `is_file` arm: zero-cost, a
    // link is its target text, never its bytes.
    let mut visit = |_key: &str, _path: &Path, meta: &std::fs::Metadata| -> Result<(), WalkError> {
        if meta.is_file() {
            measure.files += 1;
            measure.bytes += meta.len();
        }
        Ok(())
    };
    if let Err(WalkError::Other(message)) =
        walk(root, "", &mut Vec::new(), &Stop::default(), &mut visit)
    {
        return Err(message);
    }
    Ok(measure)
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
            Some(EntryKind::File { size, hash, .. }) => {
                assert_eq!(*size, 8);
                assert_eq!(*hash, ContentHash::of_bytes(b"dear all"));
            }
            other => panic!("expected a recorded file, got {other:?}"),
        }
        assert_eq!(manifest.entries.get("sent"), Some(&EntryKind::Folder));
        assert!(manifest.entries.contains_key("sent/a.txt"));
        assert_eq!(
            manifest.entries.get("empty"),
            Some(&EntryKind::Folder),
            "an empty folder must round-trip"
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
    fn hash_file_on_a_missing_path_answers_gone_rather_than_erring() {
        let dir = workshop("manifest-hash-missing");
        let missing = dir.path().join("never-existed.txt");
        assert_eq!(
            hash_file(&missing).expect("a missing file is not an error"),
            None
        );
    }

    #[test]
    fn measure_counts_files_and_bytes_without_reading_them() {
        let dir = workshop("manifest-measure");
        std::fs::write(dir.path().join("a.txt"), b"dear all").expect("fixture");
        std::fs::create_dir(dir.path().join("sub")).expect("fixture");
        std::fs::write(dir.path().join("sub").join("b.txt"), b"gone").expect("fixture");
        std::fs::create_dir(dir.path().join("empty")).expect("fixture");

        let measure = measure(dir.path()).expect("an ordinary folder measures");
        assert_eq!(measure.files, 2);
        assert_eq!(measure.bytes, 8 + 4);
    }

    /// A file gone by the time `file_entry` reads it is recorded in
    /// `unread` by its own key, not silently dropped.
    #[test]
    fn a_file_gone_at_read_time_is_recorded_unread() {
        let dir = workshop("manifest-unread-file");
        std::fs::write(dir.path().join("keep.txt"), b"kept").expect("fixture");
        std::fs::write(dir.path().join("gone.txt"), b"vanishing").expect("fixture");

        let manifest = Manifest::of_folder_via(
            dir.path(),
            &Stop::default(),
            &mut |key, path, _size, _mtime_ns| {
                if key == "gone.txt" {
                    Ok(None)
                } else {
                    hash_file(path)
                }
            },
        )
        .expect("an ordinary folder reads");

        assert!(manifest.entries.contains_key("keep.txt"));
        assert!(!manifest.entries.contains_key("gone.txt"));
        assert_eq!(manifest.unread, vec!["gone.txt".to_string()]);
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
