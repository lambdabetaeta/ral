//! What a folder holds at one moment: paths, kinds, sizes, content hashes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

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
}

impl Manifest {
    /// Read `root` into a manifest, hashing every file.
    ///
    /// # Errors
    /// A plain sentence when something in the folder cannot be read.
    pub fn of_folder(root: &Path) -> Result<Self, String> {
        Self::of_folder_via(root, &mut |_key, path, _size, _mtime_ns| hash_file(path))
    }

    /// Like [`Manifest::of_folder`], but `file_entry` decides how each
    /// file's size and hash are produced.
    pub(crate) fn of_folder_via(root: &Path, file_entry: FileEntry<'_>) -> Result<Self, String> {
        let mut entries = BTreeMap::new();
        if let Err(WalkError::Other(message)) = walk(root, "", &mut entries, file_entry) {
            return Err(message);
        }
        Ok(Self { entries })
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

/// A subfolder vanishing mid-walk is not the same as a real read failure:
/// it tells the caller to remove the `Folder` entry it just inserted and
/// move on, rather than fail the whole capture.
enum WalkError {
    Vanished,
    Other(String),
}

fn walk(
    dir: &Path,
    rel: &str,
    entries: &mut BTreeMap<String, EntryKind>,
    file_entry: FileEntry<'_>,
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
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(WalkError::Other(format!(
                    "Synod could not look at {}: {e}.",
                    path.display()
                )));
            }
        };
        if meta.file_type().is_symlink() {
            let target = match std::fs::read_link(&path) {
                Ok(target) => target,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
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
                key,
                EntryKind::Link {
                    target: target.to_string(),
                },
            );
        } else if meta.is_dir() {
            entries.insert(key.clone(), EntryKind::Folder);
            match walk(&path, &key, entries, file_entry) {
                Ok(()) => {}
                Err(WalkError::Vanished) => {
                    entries.remove(&key);
                }
                Err(e) => return Err(e),
            }
        } else if meta.is_file() {
            let size = meta.len();
            let file_mtime_ns = mtime_ns(&meta);
            if let Some((size, hash)) =
                file_entry(&key, &path, size, file_mtime_ns).map_err(WalkError::Other)?
            {
                entries.insert(
                    key,
                    EntryKind::File {
                        size,
                        hash,
                        mode: file_mode(&meta),
                        mtime_ns: file_mtime_ns,
                    },
                );
            }
        } else {
            return Err(WalkError::Other(format!(
                "{} is not an ordinary file, folder, or link, so synod could not \
                 promise to put it back; please move it out of the folder first.",
                path.display()
            )));
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
    if let Err(WalkError::Other(message)) = measure_walk(root, &mut measure) {
        return Err(message);
    }
    Ok(measure)
}

fn measure_walk(dir: &Path, out: &mut Measure) -> Result<(), WalkError> {
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
        let item = item.map_err(could_not)?;
        let path = item.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(WalkError::Other(format!(
                    "Synod could not look at {}: {e}.",
                    path.display()
                )));
            }
        };
        if meta.file_type().is_symlink() {
            // Zero-cost: a link is its target text, never its bytes.
        } else if meta.is_dir() {
            match measure_walk(&path, out) {
                Ok(()) | Err(WalkError::Vanished) => {}
                Err(e) => return Err(e),
            }
        } else if meta.is_file() {
            out.files += 1;
            out.bytes += meta.len();
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A link is its target text — never followed, so a dangling one is
    /// fine and a link to a huge file costs nothing.
    #[cfg(unix)]
    #[test]
    fn a_link_is_recorded_as_its_target_and_never_followed() {
        let dir = workshop("manifest-link");
        std::os::unix::fs::symlink("nowhere/at-all", dir.path().join("dangling")).expect("fixture");
        let manifest = Manifest::of_folder(dir.path()).expect("a dangling link must not break the walk");
        assert_eq!(
            manifest.entries.get("dangling"),
            Some(&EntryKind::Link {
                target: "nowhere/at-all".into(),
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_file_on_a_missing_path_answers_gone_rather_than_erring() {
        let dir = workshop("manifest-hash-missing");
        let missing = dir.path().join("never-existed.txt");
        assert_eq!(
            hash_file(&missing).expect("a missing file is not an error"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
