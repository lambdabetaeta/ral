//! The per-folder history store: every version of every byte, kept on the
//! host so anything a job changes can be put back.
//!
//! Layout, under synod's own state directory for the granted folder:
//! `objects/` holds file bytes named by their hash — identical content is
//! stored once, ever, so repeat checkpoints of an unchanged folder cost
//! only reads — and `checkpoints/<id>.json` holds one [`Checkpoint`] per
//! capture.  The user never sees any of this vocabulary; the GUI shows
//! only "undo" and a history.

use crate::workspace::manifest::{ContentHash, EntryKind, Manifest, hash_file};
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

/// A folder's history on the host.
///
/// Durable only for as long as the conversation that opened it:
/// [`HistoryStore::wipe`] removes it at a clean end, and [`sweep_stale`]
/// collects whatever a crash left behind.
pub struct HistoryStore {
    dir: PathBuf,
    /// A shared advisory hold on `dir`'s `lock` file, taken by
    /// [`HistoryStore::open_at`] and released only when this store drops.
    /// Its one job is visibility:
    /// [`sweep_stale`] probes the same file with a non-blocking *exclusive*
    /// lock, which a live shared hold refuses, so a store still open is a
    /// store the sweep leaves alone.
    lock: Lock,
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
        let lock = Lock::shared(&dir.join("lock"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            lock,
        })
    }

    /// Remove this store from disk, ending its conversation's undo.
    ///
    /// Releases the advisory lock first — a store cannot delete its own
    /// lock file, on Windows least of all, while still holding it open —
    /// then removes exactly the directory this store was opened at, never
    /// anything above or beside it.
    ///
    /// # Errors
    /// A plain sentence when the directory cannot be removed.
    pub fn wipe(self) -> Result<(), String> {
        let Self { dir, lock } = self;
        drop(lock);
        std::fs::remove_dir_all(&dir).map_err(|e| {
            format!(
                "Synod could not clear its safety copy at {}: {e}.",
                dir.display()
            )
        })
    }

    /// Record `root` as it stands: walk it, keep every file's bytes, and
    /// write a checkpoint.  Bytes already kept are never stored twice, and
    /// a file whose size and mtime match the folder's latest checkpoint —
    /// and whose mtime predates that checkpoint's own moment, git's racy
    /// guard — is never even reopened: its hash is already on record, and
    /// the bytes behind it are already in the store.
    ///
    /// # Errors
    /// A plain sentence when the folder cannot be read or the copy kept.
    pub fn capture(&self, root: &Path, moment: Moment) -> Result<Checkpoint, String> {
        // Stamped before the walk: the racy guard trusts an entry only when
        // its mtime predates this moment, so a file rewritten mid-walk fails
        // the trust and the next capture re-reads it.
        let taken_at_ms = now_ms();
        let reference = self.latest()?;
        let manifest = Manifest::of_folder_via(root, &mut |key, path, size, mtime_ns| {
            let reused = reference.as_ref().and_then(|reference| {
                match reference.manifest.entries.get(key) {
                    Some(EntryKind::File {
                        size: ref_size,
                        hash,
                        mtime_ns: ref_mtime_ns,
                        ..
                    }) if size == *ref_size
                        && mtime_ns == *ref_mtime_ns
                        && mtime_ns < reference.taken_at_ms * 1_000_000 =>
                    {
                        Some(hash.clone())
                    }
                    _ => None,
                }
            });
            match reused {
                Some(hash) => Ok(Some((size, hash))),
                None => self.ingest(path),
            }
        })?;
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
        let checkpoints = self.dir.join("checkpoints");
        let tmp = checkpoints.join(format!(".tmp-{id}"));
        std::fs::write(&tmp, text)
            .map_err(|e| format!("Synod could not write down what it saved: {e}."))?;
        std::fs::rename(&tmp, checkpoints.join(format!("{id}.json")))
            .map_err(|e| format!("Synod could not write down what it saved: {e}."))?;
        Ok(checkpoint)
    }

    /// The most recent checkpoint alone, parsed alone — the quick-check
    /// reference [`Self::capture`] reads once per capture. Ids order
    /// chronologically, so the latest is the greatest file stem; parsing
    /// only that one keeps a capture from re-reading every manifest this
    /// conversation has written.
    ///
    /// # Errors
    /// A plain sentence when the records cannot be listed or the latest
    /// cannot be read back.
    fn latest(&self) -> Result<Option<Checkpoint>, String> {
        let dir = self.dir.join("checkpoints");
        let could_not = |e| {
            format!(
                "Synod could not open its records at {}: {e}.",
                dir.display()
            )
        };
        let mut latest: Option<PathBuf> = None;
        for item in std::fs::read_dir(&dir).map_err(could_not)? {
            let path = item.map_err(could_not)?.path();
            if path.extension().is_some_and(|ext| ext == "json")
                && latest
                    .as_ref()
                    .is_none_or(|held| held.as_path() < path.as_path())
            {
                latest = Some(path);
            }
        }
        let Some(path) = latest else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("Synod could not read its record {}: {e}.", path.display()))?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("Synod's record {} is damaged: {e}.", path.display()))
    }

    /// Every checkpoint for this folder, oldest first.
    ///
    /// # Errors
    /// A plain sentence when a record cannot be read back.
    pub fn checkpoints(&self) -> Result<Vec<Checkpoint>, String> {
        let dir = self.dir.join("checkpoints");
        let could_not = |e| {
            format!(
                "Synod could not open its records at {}: {e}.",
                dir.display()
            )
        };
        let listing = std::fs::read_dir(&dir).map_err(could_not)?;
        let mut found = Vec::new();
        for item in listing {
            let item = item.map_err(could_not)?;
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

    /// The most recent job: its before-checkpoint, and the most recent
    /// after-checkpoint that followed it, if the run has lived long enough
    /// to take at least one.
    ///
    /// A conversation may capture several afters after its one before — one
    /// per exchange — so this pairs the before with the *last* of them, the
    /// cumulative state as of now, not the first exchange's alone.
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
            .rev()
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

    /// Copy the kept bytes for `hash` to `dest`, and set its mode.
    ///
    /// # Errors
    /// A plain sentence when the version is missing or `dest` unwritable.
    pub(crate) fn place(&self, hash: &ContentHash, mode: u32, dest: &Path) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Synod could not put back {}: {e}.", dest.display()))?;
        }
        std::fs::copy(self.object_path(hash), dest)
            .map_err(|e| format!("Synod could not put back {}: {e}.", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
                .map_err(|e| format!("Synod could not put back {}: {e}.", dest.display()))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    }

    /// Keep one file's bytes; free when identical bytes are already kept.
    ///
    /// The rare miss copies while re-hashing, so the stored bytes always
    /// match the name they are stored under even if the file changes
    /// beneath us mid-capture.  The tmp file is named per call, not per
    /// content, since two ingests of identical bytes can run at once; a
    /// failure past this point removes it rather than stranding it.  A
    /// file gone by the time it is opened — here or in the read this
    /// follows — is not an error: it answers `Ok(None)`, the same as a
    /// deletion the walk noticed itself.
    fn ingest(&self, file: &Path) -> Result<Option<(u64, ContentHash)>, String> {
        let Some((size, hash)) = hash_file(file)? else {
            return Ok(None);
        };
        if self.object_path(&hash).exists() {
            return Ok(Some((size, hash)));
        }

        let could_not = |e| format!("Synod could not keep a copy of {}: {e}.", file.display());
        let mut src = match std::fs::File::open(file) {
            Ok(src) => src,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(could_not(e)),
        };
        let tmp = self.dir.join("objects").join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut out = std::fs::File::create(&tmp).map_err(could_not)?;

        let result = (|| {
            let mut hasher = blake3::Hasher::new();
            let size =
                std::io::copy(&mut src, &mut Tee(&mut hasher, &mut out)).map_err(could_not)?;
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
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(Some)
    }

    /// This store's own directory on the host — the volume a free-space
    /// check weighs a folder's bytes against.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn object_path(&self, hash: &ContentHash) -> PathBuf {
        let hex = hash.as_str();
        self.dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }
}

/// Remove every history store on this computer that nothing has open.
///
/// The startup counterpart to [`HistoryStore::wipe`]'s clean-exit removal,
/// for whatever a crashed run left behind. Call once, before any
/// conversation can begin.
pub fn sweep_stale() {
    sweep_stale_under(&crate::session::SYNOD.xdg_dir(ral_core::path::basedir::XdgKind::State));
}

/// [`sweep_stale`]'s body, over an explicit state root so a test can point
/// it at a temporary one instead of this computer's real synod state.
///
/// Best effort throughout: a `<slug>/history` directory this cannot read,
/// lock, or remove is left for the next start to retry, never reported as
/// an error here. Only a directory literally named `history` immediately
/// under one `<slug>/` is ever touched — the run logs and model selection
/// beside it belong to another door entirely.
fn sweep_stale_under(state_root: &Path) {
    let Ok(slugs) = std::fs::read_dir(state_root) else {
        return;
    };
    for slug in slugs.flatten() {
        let history = slug.path().join("history");
        if history.is_dir() {
            let _ = sweep_one(&history);
        }
    }
}

/// Sweep one `history` directory: gone if its `lock` file's exclusive
/// probe succeeds (nobody holds it), left untouched otherwise.
fn sweep_one(history: &Path) -> Result<(), String> {
    let Some(lock) = Lock::try_exclusive(&history.join("lock"))? else {
        return Ok(());
    };
    let entries = std::fs::read_dir(history)
        .map_err(|e| format!("Synod could not read {}: {e}.", history.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("Synod could not read {}: {e}.", history.display()))?;
        if entry.file_name() == "lock" {
            continue;
        }
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        result.map_err(|e| format!("Synod could not clear {}: {e}.", path.display()))?;
    }
    // The lock file cannot go while it is still open — on Windows least of
    // all — so the probe's own hold is dropped before either removal below.
    drop(lock);
    let _ = std::fs::remove_file(history.join("lock"));
    std::fs::remove_dir(history)
        .map_err(|e| format!("Synod could not clear {}: {e}.", history.display()))
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

/// An advisory hold on one file, taken either shared (a live store's own
/// hold, held for its whole life) or exclusive-and-non-blocking
/// ([`sweep_stale`]'s probe: acquired means nothing else holds the file at
/// all, refused means a live shared hold is out there). Dropping it
/// releases the hold and closes the underlying handle — the field is never
/// read, only kept alive.
struct Lock(#[allow(dead_code)] lock_imp::Handle);

impl Lock {
    /// Take `path` (creating it if needed) with a shared advisory lock,
    /// blocking only as long as some other exclusive prober is mid-check —
    /// never against another store's own shared hold, since shared holds
    /// never contend with each other.
    fn shared(path: &Path) -> Result<Self, String> {
        lock_imp::Handle::shared(path).map(Self)
    }

    /// Try `path` with a non-blocking exclusive lock. `Ok(None)` means a
    /// live shared hold refused it; `Ok(Some(_))` means nothing held it at
    /// all, and the returned guard now does, until dropped.
    fn try_exclusive(path: &Path) -> Result<Option<Self>, String> {
        Ok(lock_imp::Handle::try_exclusive(path)?.map(Self))
    }
}

#[cfg(unix)]
mod lock_imp {
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    pub struct Handle(#[allow(dead_code)] std::fs::File);

    impl Handle {
        pub fn shared(path: &Path) -> Result<Self, String> {
            let file = open(path)?;
            flock(&file, libc::LOCK_SH)
                .map_err(|e| format!("Synod could not lock {}: {e}.", path.display()))?;
            Ok(Self(file))
        }

        pub fn try_exclusive(path: &Path) -> Result<Option<Self>, String> {
            let file = open(path)?;
            match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
                Ok(()) => Ok(Some(Self(file))),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(format!("Synod could not lock {}: {e}.", path.display())),
            }
        }
    }

    fn open(path: &Path) -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| format!("Synod could not open {}: {e}.", path.display()))
    }

    fn flock(file: &std::fs::File, op: i32) -> io::Result<()> {
        // SAFETY: `file`'s descriptor is valid for the call's duration, and
        // `flock` neither reads nor writes through it.
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(windows)]
mod lock_imp {
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    pub struct Handle(#[allow(dead_code)] std::fs::File);

    impl Handle {
        pub fn shared(path: &Path) -> Result<Self, String> {
            let file = open(path)?;
            lock(&file, 0).map_err(|e| format!("Synod could not lock {}: {e}.", path.display()))?;
            Ok(Self(file))
        }

        pub fn try_exclusive(path: &Path) -> Result<Option<Self>, String> {
            let file = open(path)?;
            match lock(&file, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY) {
                Ok(()) => Ok(Some(Self(file))),
                Err(e) if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) => Ok(None),
                Err(e) => Err(format!("Synod could not lock {}: {e}.", path.display())),
            }
        }
    }

    fn open(path: &Path) -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| format!("Synod could not open {}: {e}.", path.display()))
    }

    fn lock(file: &std::fs::File, flags: u32) -> std::io::Result<()> {
        // Whole-file range: `!0u32` on both halves is the largest region
        // `LockFileEx` can name, wide enough to cover a lock file that is
        // always empty in practice.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // SAFETY: `file`'s handle is valid for the call's duration, and
        // `overlapped` is a plain (non-async) zeroed region descriptor.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle().cast(),
                flags,
                0,
                !0u32,
                !0u32,
                &raw mut overlapped,
            )
        };
        if ok != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// Free space on the filesystem holding `path`, or `None` when it cannot be
/// learned — never an error, since this only feeds a warning rather than a
/// decision.
#[cfg(unix)]
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &raw mut stat) };
    if rc != 0 {
        return None;
    }
    // `f_frsize` is already `u64` on every unix this crate targets; only
    // `f_bavail` narrows on some (macOS's is 32 bits).
    Some(u64::from(stat.f_bavail) * stat.f_frsize)
}

#[cfg(windows)]
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free_to_caller: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer, and the two output
    // pointers we do not need are null, which the API accepts.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free_to_caller)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::granted_workshop as workshop;
    use crate::test_fixture::workshop as bare_workshop;
    use crate::workspace::manifest::EntryKind;

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
        let (_dir, folder, store) = workshop("history-capture");
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
    }

    #[test]
    fn unchanged_bytes_are_never_stored_twice() {
        let (dir, folder, store) = workshop("history-dedupe");
        std::fs::write(folder.join("a.txt"), b"stable").expect("fixture");
        std::fs::write(folder.join("b.txt"), b"stable").expect("fixture");

        store
            .capture(&folder, Moment::Before)
            .expect("first capture");
        assert_eq!(object_count(dir.path()), 1, "identical files share one object");
        store
            .capture(&folder, Moment::After)
            .expect("second capture");
        assert_eq!(object_count(dir.path()), 1, "a repeat capture adds nothing");
    }

    #[test]
    fn the_latest_job_pairs_before_with_its_after() {
        let (_dir, folder, store) = workshop("history-pairing");
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
    }

    /// A conversation captures one before and many afters, one per
    /// exchange. `latest_job` must pair the before with the *last* after —
    /// the cumulative state as of now — not the first exchange's alone,
    /// which is all a plain `find` would ever see.
    #[test]
    fn a_conversations_many_afters_pair_with_the_before_by_the_last_one() {
        let (_dir, folder, store) = workshop("history-many-afters");
        std::fs::write(folder.join("a.txt"), b"one").expect("fixture");
        let before = store.capture(&folder, Moment::Before).expect("captures");
        store.capture(&folder, Moment::After).expect("exchange 1");
        store.capture(&folder, Moment::After).expect("exchange 2");
        let last = store.capture(&folder, Moment::After).expect("exchange 3");

        let (paired_before, paired_after) =
            store.latest_job().expect("reads").expect("a job exists");
        assert_eq!(paired_before.id, before.id);
        assert_eq!(
            paired_after
                .expect("a conversation with three exchanges has an after")
                .id,
            last.id,
            "the report must reflect the most recent exchange, not the first"
        );
    }

    #[test]
    fn checkpoints_come_back_oldest_first() {
        let (_dir, folder, store) = workshop("history-order");
        std::fs::write(folder.join("a.txt"), b"x").expect("fixture");
        let first = store.capture(&folder, Moment::Before).expect("captures");
        let second = store.capture(&folder, Moment::After).expect("captures");

        let all = store.checkpoints().expect("reads");
        assert_eq!(
            all.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![first.id.as_str(), second.id.as_str()]
        );
    }

    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("fixture opens for mtime");
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("fixture sets mtime");
    }

    fn file_hash(manifest: &Manifest, key: &str) -> ContentHash {
        match manifest.entries.get(key) {
            Some(EntryKind::File { hash, .. }) => hash.clone(),
            other => panic!("expected a recorded file at {key}, got {other:?}"),
        }
    }

    /// A file whose stat matches the reference checkpoint, taken well
    /// before it, is never reopened: its object is already in the store.
    /// Deleting that object out from under a quick-checked capture and
    /// confirming it stays deleted is the only honest way to prove the
    /// bytes were never re-read.
    #[test]
    fn a_stat_match_older_than_the_reference_is_reused_without_reopening() {
        let (_dir, folder, store) = workshop("history-quick-check-reuse");
        std::fs::write(folder.join("a.txt"), b"dear all").expect("fixture");
        set_mtime(
            &folder.join("a.txt"),
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_hours(365 * 24),
        );

        let first = store.capture(&folder, Moment::Before).expect("captures");
        let hash = file_hash(&first.manifest, "a.txt");
        let object = store.object_path(&hash);
        assert!(
            object.exists(),
            "the first capture must have stored the bytes"
        );
        std::fs::remove_file(&object).expect("fixture removes the object");

        let second = store.capture(&folder, Moment::After).expect("captures");
        assert_eq!(
            file_hash(&second.manifest, "a.txt"),
            hash,
            "an unchanged stat must reuse the recorded hash"
        );
        assert!(
            !object.exists(),
            "a quick-checked file must never be reopened, so its object stays gone"
        );
    }

    /// Git's racy guard: a file stamped at or after the reference
    /// checkpoint's own moment could have been rewritten within the same
    /// tick, so an unchanged size and mtime are not enough — it is
    /// reread regardless.
    #[test]
    fn a_stat_match_not_older_than_the_reference_is_reread_regardless() {
        let (_dir, folder, store) = workshop("history-quick-check-racy");
        let path = folder.join("a.txt");
        std::fs::write(&path, b"one-two").expect("fixture");
        let future = std::time::SystemTime::now() + std::time::Duration::from_hours(1);
        set_mtime(&path, future);

        let first = store.capture(&folder, Moment::Before).expect("captures");
        let first_hash = file_hash(&first.manifest, "a.txt");

        std::fs::write(&path, b"two-one").expect("fixture");
        set_mtime(&path, future);

        let second = store.capture(&folder, Moment::After).expect("captures");
        let second_hash = file_hash(&second.manifest, "a.txt");
        assert_ne!(
            second_hash, first_hash,
            "a same-tick rewrite must not hide behind an unchanged stat"
        );
        assert_eq!(second_hash, ContentHash::of_bytes(b"two-one"));
    }

    #[test]
    fn wipe_removes_the_store_and_nothing_beside_it() {
        let (dir, folder, store) = workshop("history-wipe");
        std::fs::write(folder.join("a.txt"), b"kept once").expect("fixture");
        store.capture(&folder, Moment::Before).expect("captures");
        let sibling = dir.path().join("run.log");
        std::fs::write(&sibling, b"a run log, not this store's business").expect("fixture");

        store.wipe().expect("wipes");

        assert!(
            !dir.path().join("history").exists(),
            "the store's own directory must be gone"
        );
        assert!(
            sibling.exists(),
            "a wipe must never touch anything beside its own directory"
        );
    }

    #[test]
    fn two_stores_over_the_same_directory_coexist() {
        let dir = bare_workshop("history-shared-shared");
        let first = HistoryStore::open_at(dir.path()).expect("first store opens");
        let second = HistoryStore::open_at(dir.path()).expect("a second shared open must not refuse");

        drop(first);
        drop(second);
    }

    #[test]
    fn sweep_removes_an_unheld_store_and_skips_a_held_one() {
        let root = bare_workshop("history-sweep-root");
        let held_history = root.path().join("held").join("history");
        let free_history = root.path().join("free").join("history");
        // Held: its `HistoryStore` stays alive across the sweep below, so
        // its shared lock is still out there for the sweep's exclusive
        // probe to refuse.
        let held_store = HistoryStore::open_at(&held_history).expect("held store opens");
        // Free: opened and dropped before the sweep runs, so nothing holds
        // its lock any more — exactly what a crashed run leaves behind.
        drop(HistoryStore::open_at(&free_history).expect("free store opens"));

        sweep_stale_under(root.path());

        assert!(
            held_history.exists(),
            "a store a live `HistoryStore` still holds must survive the sweep"
        );
        assert!(
            !free_history.exists(),
            "a store nobody holds must be swept away"
        );

        drop(held_store);
    }

    #[test]
    fn a_swept_slugs_siblings_outside_history_survive() {
        let root = bare_workshop("history-sweep-siblings");
        let slug = root.path().join("a-project");
        let free_history = slug.join("history");
        HistoryStore::open_at(&free_history).expect("store opens");
        let sibling = slug.join("run.log");
        std::fs::write(&sibling, b"another door's business").expect("fixture");

        sweep_stale_under(root.path());

        assert!(!free_history.exists(), "the unheld store must be swept");
        assert!(
            sibling.exists(),
            "a sibling file beside `history/` is not the sweep's to touch"
        );
    }
}
