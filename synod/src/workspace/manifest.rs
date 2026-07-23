//! What a folder holds at one moment: paths, kinds, sizes, content hashes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Warn before checkpointing a folder bigger than this.
///
/// A checkpoint reads every byte, before and after each job.  2 GiB keeps
/// that in the seconds on a local disk; past it — especially on a
/// departmental share at tens of MB/s — the wait reaches minutes and the
/// user deserves a heads-up before it starts.
pub const LARGE_FOLDER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How a walk turns a file into its size and hash — the history store
/// hooks in here to keep the bytes while it hashes them.
pub(crate) type FileEntry<'a> = &'a mut dyn FnMut(&Path) -> Result<(u64, ContentHash), String>;

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

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry in a [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File {
        size: u64,
        hash: ContentHash,
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
        Self::of_folder_via(root, &mut hash_file)
    }

    /// Like [`Manifest::of_folder`], but `file_entry` decides how each
    /// file's size and hash are produced.
    pub(crate) fn of_folder_via(root: &Path, file_entry: FileEntry<'_>) -> Result<Self, String> {
        let mut entries = BTreeMap::new();
        walk(root, "", &mut entries, file_entry)?;
        Ok(Self { entries })
    }
}

/// Hash a file's bytes, streaming.
///
/// # Errors
/// A plain sentence when the file cannot be read.
pub fn hash_file(path: &Path) -> Result<(u64, ContentHash), String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("Synod could not read {}: {e}.", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let size = std::io::copy(&mut file, &mut hasher)
        .map_err(|e| format!("Synod could not read {}: {e}.", path.display()))?;
    Ok((size, ContentHash::from(hasher.finalize())))
}

fn walk(
    dir: &Path,
    rel: &str,
    entries: &mut BTreeMap<String, EntryKind>,
    file_entry: FileEntry<'_>,
) -> Result<(), String> {
    let listing = std::fs::read_dir(dir)
        .map_err(|e| format!("Synod could not look inside {}: {e}.", dir.display()))?;
    for item in listing {
        let item =
            item.map_err(|e| format!("Synod could not look inside {}: {e}.", dir.display()))?;
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            return Err(format!(
                "A file in {} has a name this computer cannot read; please rename it first.",
                dir.display()
            ));
        };
        let key = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        let path = item.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("Synod could not look at {}: {e}.", path.display()))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(|e| format!("Synod could not read the link {}: {e}.", path.display()))?;
            let Some(target) = target.to_str() else {
                return Err(format!(
                    "The link {} points at a name this computer cannot read; \
                     please fix or remove it first.",
                    path.display()
                ));
            };
            entries.insert(
                key,
                EntryKind::Link {
                    target: target.to_string(),
                },
            );
        } else if meta.is_dir() {
            entries.insert(key.clone(), EntryKind::Folder);
            walk(&path, &key, entries, file_entry)?;
        } else if meta.is_file() {
            let (size, hash) = file_entry(&path)?;
            entries.insert(key, EntryKind::File { size, hash });
        } else {
            return Err(format!(
                "{} is not an ordinary file, folder, or link, so synod could not \
                 promise to put it back; please move it out of the folder first.",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A private directory for one test, named after it so a failed run
    /// leaves readable wreckage.
    fn workshop(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("synod-manifest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp workshop");
        std::fs::canonicalize(&dir).expect("temp workshop resolves")
    }

    #[test]
    fn files_folders_and_empty_folders_are_all_recorded() {
        let dir = workshop("record");
        std::fs::write(dir.join("letter.txt"), b"dear all").expect("fixture");
        std::fs::create_dir(dir.join("sent")).expect("fixture");
        std::fs::write(dir.join("sent").join("a.txt"), b"gone").expect("fixture");
        std::fs::create_dir(dir.join("empty")).expect("fixture");

        let manifest = Manifest::of_folder(&dir).expect("an ordinary folder reads");
        assert_eq!(
            manifest.entries.get("letter.txt"),
            Some(&EntryKind::File {
                size: 8,
                hash: ContentHash::of_bytes(b"dear all"),
            })
        );
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
        let dir = workshop("link");
        std::os::unix::fs::symlink("nowhere/at-all", dir.join("dangling")).expect("fixture");
        let manifest = Manifest::of_folder(&dir).expect("a dangling link must not break the walk");
        assert_eq!(
            manifest.entries.get("dangling"),
            Some(&EntryKind::Link {
                target: "nowhere/at-all".into(),
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
