//! The per-folder history store: every version of every byte, kept on the
//! host so anything a job changes can be put back.
//!
//! Layout, under synod's own state directory for the granted folder:
//! `objects/` holds file bytes named by their hash — identical content is
//! stored once, ever, so repeat checkpoints of an unchanged folder cost
//! only reads — and `checkpoints/<id>.json` holds one [`Checkpoint`] per
//! capture.  The user never sees any of this vocabulary; the GUI shows
//! only "undo" and a history.

use crate::workspace::manifest::{ContentHash, Manifest, hash_file};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// When a checkpoint was taken, relative to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Moment {
    /// The baseline a job is judged against and undone to.
    Before,
    /// What the job left behind; the conflict check's reference point.
    After,
    /// The folder as it stood when an undo began — the safety net that
    /// makes every undo itself reversible in principle.
    Undo,
}

/// One recorded state of the folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Sortable and unique: milliseconds, process, sequence.
    pub id: String,
    pub taken_at_ms: u64,
    pub moment: Moment,
    /// The granted folder this records, as a host path.
    pub folder: String,
    pub manifest: Manifest,
}

/// A folder's history on the host, durable across sessions.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    dir: PathBuf,
}

/// Orders captures taken within the same millisecond by one process.
static SEQ: AtomicU64 = AtomicU64::new(0);

impl HistoryStore {
    /// The store for `folder`, under synod's own state directory.
    ///
    /// # Errors
    /// A plain sentence when the store's directories cannot be made.
    pub fn open_for(folder: &Path) -> Result<Self, String> {
        Self::open_at(
            &crate::session::SYNOD
                .project_dir(&folder.to_string_lossy())
                .join("history"),
        )
    }

    /// Open (creating if needed) a store at an explicit location.
    ///
    /// # Errors
    /// A plain sentence when the store's directories cannot be made.
    pub fn open_at(dir: &Path) -> Result<Self, String> {
        for sub in ["objects", "checkpoints"] {
            std::fs::create_dir_all(dir.join(sub)).map_err(|e| {
                format!(
                    "Synod could not make its safety-copy area at {}: {e}.",
                    dir.display()
                )
            })?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// Record `root` as it stands: walk it, keep every file's bytes, and
    /// write a checkpoint.  Bytes already kept are never stored twice.
    ///
    /// # Errors
    /// A plain sentence when the folder cannot be read or the copy kept.
    pub fn capture(&self, root: &Path, moment: Moment) -> Result<Checkpoint, String> {
        let manifest = Manifest::of_folder_via(root, &mut |file| self.ingest(file))?;
        let taken_at_ms = now_ms();
        let id = format!(
            "{taken_at_ms:015}-{:010}-{:06}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let checkpoint = Checkpoint {
            id: id.clone(),
            taken_at_ms,
            moment,
            folder: root.to_string_lossy().into_owned(),
            manifest,
        };
        let text = serde_json::to_string(&checkpoint)
            .map_err(|e| format!("Synod could not write down what it saved: {e}."))?;
        let path = self.dir.join("checkpoints").join(format!("{id}.json"));
        std::fs::write(&path, text)
            .map_err(|e| format!("Synod could not write down what it saved: {e}."))?;
        Ok(checkpoint)
    }

    /// Every checkpoint for this folder, oldest first.
    ///
    /// # Errors
    /// A plain sentence when a record cannot be read back.
    pub fn checkpoints(&self) -> Result<Vec<Checkpoint>, String> {
        let dir = self.dir.join("checkpoints");
        let listing = std::fs::read_dir(&dir).map_err(|e| {
            format!(
                "Synod could not open its records at {}: {e}.",
                dir.display()
            )
        })?;
        let mut found = Vec::new();
        for item in listing {
            let item = item.map_err(|e| {
                format!(
                    "Synod could not open its records at {}: {e}.",
                    dir.display()
                )
            })?;
            let path = item.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("Synod could not read its record {}: {e}.", path.display()))?;
            let checkpoint: Checkpoint = serde_json::from_str(&text)
                .map_err(|e| format!("Synod's record {} is damaged: {e}.", path.display()))?;
            found.push(checkpoint);
        }
        found.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(found)
    }

    /// The most recent job: its before-checkpoint, and the after one when
    /// the run lived long enough to take it.
    ///
    /// # Errors
    /// A plain sentence when the records cannot be read back.
    pub fn latest_job(&self) -> Result<Option<(Checkpoint, Option<Checkpoint>)>, String> {
        let all = self.checkpoints()?;
        let Some(at) = all.iter().rposition(|c| c.moment == Moment::Before) else {
            return Ok(None);
        };
        let after = all[at + 1..]
            .iter()
            .find(|c| c.moment == Moment::After)
            .cloned();
        Ok(Some((all[at].clone(), after)))
    }

    /// The kept bytes for `hash`.
    ///
    /// # Errors
    /// A plain sentence when that version is no longer in the store.
    pub fn read_object(&self, hash: &ContentHash) -> Result<Vec<u8>, String> {
        std::fs::read(self.object_path(hash))
            .map_err(|e| format!("Synod no longer has a saved copy of that version: {e}."))
    }

    /// Copy the kept bytes for `hash` to `dest`, replacing what is there.
    ///
    /// # Errors
    /// A plain sentence when the version is missing or `dest` unwritable.
    pub(crate) fn place(&self, hash: &ContentHash, dest: &Path) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Synod could not put back {}: {e}.", dest.display()))?;
        }
        if std::fs::symlink_metadata(dest).is_ok() {
            std::fs::remove_file(dest)
                .map_err(|e| format!("Synod could not put back {}: {e}.", dest.display()))?;
        }
        std::fs::copy(self.object_path(hash), dest)
            .map_err(|e| format!("Synod could not put back {}: {e}.", dest.display()))?;
        Ok(())
    }

    /// Keep one file's bytes; free when identical bytes are already kept.
    ///
    /// The rare miss copies while re-hashing, so the stored bytes always
    /// match the name they are stored under even if the file changes
    /// beneath us mid-capture.
    fn ingest(&self, file: &Path) -> Result<(u64, ContentHash), String> {
        let (size, hash) = hash_file(file)?;
        if self.object_path(&hash).exists() {
            return Ok((size, hash));
        }

        let could_not = |e| format!("Synod could not keep a copy of {}: {e}.", file.display());
        let tmp =
            self.dir
                .join("objects")
                .join(format!(".tmp-{}-{}", std::process::id(), hash.as_str()));
        let mut src = std::fs::File::open(file).map_err(could_not)?;
        let mut out = std::fs::File::create(&tmp).map_err(could_not)?;
        let mut hasher = blake3::Hasher::new();
        let size = std::io::copy(&mut src, &mut Tee(&mut hasher, &mut out)).map_err(could_not)?;
        drop(out);

        let hash = ContentHash::from(hasher.finalize());
        let dest = self.object_path(&hash);
        if dest.exists() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(could_not)?;
            }
            std::fs::rename(&tmp, &dest).map_err(could_not)?;
        }
        Ok((size, hash))
    }

    fn object_path(&self, hash: &ContentHash) -> PathBuf {
        let hex = hash.as_str();
        self.dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }
}

/// Writes to a file while feeding the same bytes to a hasher.
struct Tee<'a>(&'a mut blake3::Hasher, &'a mut std::fs::File);

impl Write for Tee<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.1.write(buf)?;
        self.0.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.1.flush()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::manifest::EntryKind;

    /// A workshop holding a granted folder and a store, side by side.
    fn workshop(tag: &str) -> (PathBuf, PathBuf, HistoryStore) {
        let dir = std::env::temp_dir().join(format!("synod-history-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = dir.join("Admissions");
        std::fs::create_dir_all(&folder).expect("temp workshop");
        let store = HistoryStore::open_at(&dir.join("history")).expect("store opens");
        let dir = std::fs::canonicalize(&dir).expect("temp workshop resolves");
        (dir.clone(), dir.join("Admissions"), store)
    }

    fn object_count(dir: &Path) -> usize {
        let mut count = 0;
        for shard in std::fs::read_dir(dir.join("history").join("objects")).expect("objects dir") {
            let shard = shard.expect("shard entry").path();
            if shard.is_dir() {
                count += std::fs::read_dir(shard).expect("shard reads").count();
            }
        }
        count
    }

    #[test]
    fn a_capture_keeps_the_bytes_and_the_shape() {
        let (dir, folder, store) = workshop("capture");
        std::fs::write(folder.join("letter.txt"), b"dear all").expect("fixture");
        std::fs::create_dir(folder.join("empty")).expect("fixture");

        let checkpoint = store.capture(&folder, Moment::Before).expect("captures");
        assert_eq!(checkpoint.moment, Moment::Before);
        assert_eq!(
            checkpoint.manifest.entries.get("empty"),
            Some(&EntryKind::Folder)
        );
        assert_eq!(
            store
                .read_object(&ContentHash::of_bytes(b"dear all"))
                .expect("bytes kept"),
            b"dear all",
            "the file's bytes must be recoverable from the store"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unchanged_bytes_are_never_stored_twice() {
        let (dir, folder, store) = workshop("dedupe");
        std::fs::write(folder.join("a.txt"), b"stable").expect("fixture");
        std::fs::write(folder.join("b.txt"), b"stable").expect("fixture");

        store
            .capture(&folder, Moment::Before)
            .expect("first capture");
        assert_eq!(object_count(&dir), 1, "identical files share one object");
        store
            .capture(&folder, Moment::After)
            .expect("second capture");
        assert_eq!(object_count(&dir), 1, "a repeat capture adds nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_latest_job_pairs_before_with_its_after() {
        let (dir, folder, store) = workshop("pairing");
        std::fs::write(folder.join("a.txt"), b"one").expect("fixture");
        let b1 = store.capture(&folder, Moment::Before).expect("captures");
        let a1 = store.capture(&folder, Moment::After).expect("captures");
        store.capture(&folder, Moment::Undo).expect("captures");

        let (before, after) = store.latest_job().expect("reads").expect("a job exists");
        assert_eq!(before.id, b1.id);
        assert_eq!(after.expect("the run finished").id, a1.id);

        // A run that died before its closing checkpoint has no after.
        let b2 = store.capture(&folder, Moment::Before).expect("captures");
        let (before, after) = store.latest_job().expect("reads").expect("a job exists");
        assert_eq!(before.id, b2.id);
        assert!(after.is_none(), "a crashed run left no after-checkpoint");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoints_come_back_oldest_first() {
        let (dir, folder, store) = workshop("order");
        std::fs::write(folder.join("a.txt"), b"x").expect("fixture");
        let first = store.capture(&folder, Moment::Before).expect("captures");
        let second = store.capture(&folder, Moment::After).expect("captures");

        let all = store.checkpoints().expect("reads");
        assert_eq!(
            all.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![first.id.as_str(), second.id.as_str()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
