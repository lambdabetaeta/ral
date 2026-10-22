//! What a folder holds at one moment: paths, kinds, sizes, timestamps.
//!
//! Nothing here ever opens a file.  A manifest is built from what the tree
//! walk's listing states of each entry and nothing else, which is what makes
//! opening a folder cost one stat-walk regardless of how many gigabytes are in
//! it — and what makes [`Change::Touched`](crate::workspace::changes::Change)
//! necessary, since a walk that never reads bytes cannot claim they differ.
//!
//! Its keys are lexical history: the names the walk reached each entry by,
//! past the root, slash-joined.  [`Manifest::of_folder_via`] is the one door;
//! the folder is the user's own and the walk is judged by the root stack.

use crate::grant::spelled;
use ral_core::capability::Ungoverned;
use ral_core::path::tree::walk::{Found, Unentered, Walk, WalkEntry};
use ral_core::path::{Kind, LeafMode, LexicalPath, Stat, relative_key, shown, walk};
use ral_core::process::{CancelCause, DurableRoot, Scope};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

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
    /// manifest reads bytes, that case cannot be settled — so the
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
    #[allow(
        clippy::disallowed_methods,
        reason = "path-form: the folder a message names for an entry whose name this computer cannot read"
    )]
    pub fn of_folder_via(root: &Path, stop: &Stop, progress: Progress<'_>) -> Result<Self, String> {
        let named = spelled(root)?;
        let mut manifest = Self::default();
        let mut files: u64 = 0;
        for entry in Walk::all(&named)
            .readable_by(Ungoverned::region())
            .entries(stop.scope())
        {
            let entry = entry.map_err(|_| {
                "Synod stopped reading this folder before it had finished, so it cannot say what \
                 changed."
                    .to_string()
            })?;
            if entry.depth == 0 {
                entered(&entry, root)?;
                continue;
            }
            let key = relative_key(&entry.shown, &named).ok_or_else(|| {
                format!(
                    "A file in {} has a name this computer cannot read; please rename it first.",
                    shown(entry.shown.as_path().parent().unwrap_or(root))
                )
            })?;
            if manifest.record(&entry, key)? {
                files += 1;
                progress(files);
            }
        }
        Ok(manifest)
    }

    /// Record what the walk found at `key`; whether it was a file.
    fn record(&mut self, entry: &WalkEntry, key: String) -> Result<bool, String> {
        let unreadable = |e: &io::Error, what: &str| {
            format!(
                "Synod could not {what} {}: {e}.",
                shown(entry.shown.as_path())
            )
        };
        let kind = match &entry.found {
            Found::Seen(stat) => match stat.kind {
                Kind::Dir => EntryKind::Folder,
                Kind::File => EntryKind::File {
                    size: stat.len,
                    mtime_ns: mtime_ns(stat),
                    mode: file_mode(stat),
                },
                Kind::Symlink => match link_target(&entry.shown) {
                    Ok(target) => EntryKind::Link {
                        target: target_text(&target, entry)?,
                    },
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        record_unread(&mut self.unread, key);
                        return Ok(false);
                    }
                    Err(e) => return Err(unreadable(&e, "read the link")),
                },
                Kind::Other => {
                    return Err(format!(
                        "{} is not an ordinary file, folder, or link, so synod could not \
                         account for it; please move it out of the folder first.",
                        shown(entry.shown.as_path())
                    ));
                }
            },
            // Listed, then gone before it could be looked at, or a folder that
            // went before it could be entered: unread, not absent.
            Found::Unseen(e)
            | Found::Unentered {
                why: Unentered::Refused(e),
                ..
            } if e.kind() == io::ErrorKind::NotFound => {
                record_unread(&mut self.unread, key);
                return Ok(false);
            }
            Found::Unseen(e) => return Err(unreadable(e, "look at")),
            Found::Unentered {
                why: Unentered::Refused(e),
                ..
            } => return Err(unreadable(e, "look inside")),
            Found::Unentered {
                why: Unentered::Pruned,
                ..
            } => unreachable!("a walk with no depth or mount limit prunes nothing"),
        };
        let file = matches!(kind, EntryKind::File { .. });
        self.entries.insert(key, kind);
        Ok(file)
    }
}

/// The root, which is no entry: a root that cannot be entered fails the
/// manifest whole, since an empty one would say the folder held nothing, and
/// every file in the other manifest would answer to that as a deletion.
fn entered(root: &WalkEntry, named: &Path) -> Result<(), String> {
    match &root.found {
        Found::Seen(Stat {
            kind: Kind::Dir, ..
        }) => Ok(()),
        Found::Unseen(e)
        | Found::Unentered {
            why: Unentered::Refused(e),
            ..
        } if e.kind() == io::ErrorKind::NotFound => Err(format!(
            "Synod could not find {} any more — has the folder been moved, renamed, \
             or unplugged?",
            shown(named)
        )),
        Found::Unseen(e)
        | Found::Unentered {
            why: Unentered::Refused(e),
            ..
        } => Err(format!(
            "Synod could not look inside {}: {e}.",
            shown(named)
        )),
        _ => Err(format!(
            "Synod could not look inside {}: it is not a folder.",
            shown(named)
        )),
    }
}

/// A link's target as the text it is recorded by.
fn target_text(target: &Path, link: &WalkEntry) -> Result<String, String> {
    #[allow(
        clippy::disallowed_methods,
        reason = "native: a link target is recorded as text, so one that is not Unicode is \
                  refused"
    )]
    target.to_str().map(str::to_string).ok_or_else(|| {
        format!(
            "The link {} points at a name this computer cannot read; \
             please fix or remove it first.",
            shown(link.shown.as_path())
        )
    })
}

/// The target of the link at `at`, read through the held leaf.  Ungoverned:
/// the folder is the user's own, and the manifest is synod's bookkeeping, not
/// the model's I/O.
fn link_target(at: &LexicalPath) -> io::Result<PathBuf> {
    Ungoverned::read(walk(at, LeafMode::NoFollow)?).read_link()
}

/// A file's Unix permission bits; `0` on a host with no such notion.
fn file_mode(stat: &Stat) -> u32 {
    #[cfg(unix)]
    return stat.mode & 0o7777;
    #[cfg(not(unix))]
    return {
        let _ = stat;
        0
    };
}

/// A file's modification time, in nanoseconds since the Unix epoch; `0`
/// when the platform or filesystem yields none.
fn mtime_ns(stat: &Stat) -> u64 {
    stat.mtime
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
/// is not the same as stopping it.  The walk polls it as its cancel scope.
#[derive(Clone, Default)]
pub struct Stop(DurableRoot);

impl Stop {
    /// Ask the walk to stop at its next entry. Nothing resets this: a
    /// stopped walk is abandoned, never resumed.
    pub fn stop(&self) {
        self.0.cancel(CancelCause::Interrupted);
    }

    fn scope(&self) -> Scope {
        Scope::clone(&self.0)
    }
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

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] the files and folders the walk is then asked to record"
)]
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

    /// An empty manifest would say the folder held nothing, and every file
    /// in the other manifest would answer to that as a deletion.
    #[test]
    fn a_root_that_cannot_be_entered_fails_the_manifest() {
        let dir = workshop("manifest-root");
        std::fs::write(dir.path().join("file"), b"x").expect("fixture");
        assert!(Manifest::of_folder(&dir.path().join("gone")).is_err());
        assert!(Manifest::of_folder(&dir.path().join("file")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.path().join("locked");
            std::fs::create_dir(&locked).expect("fixture");
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
                .expect("fixture");
            // Root lists a `0000` directory; the case binds only where the
            // permission does.
            let refused = std::fs::read_dir(&locked).is_err();
            let result = Manifest::of_folder(&locked);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
                .expect("fixture");
            assert!(!refused || result.is_err());
        }
    }

    /// Listed, then gone: unread, never a deletion and never a failure.
    #[test]
    fn a_subfolder_that_vanished_is_unread() {
        let dir = workshop("manifest-vanished");
        std::fs::write(dir.path().join("a.txt"), b"dear all").expect("fixture");
        std::fs::create_dir(dir.path().join("sub")).expect("fixture");
        std::fs::write(dir.path().join("sub").join("x.txt"), b"gone").expect("fixture");

        let manifest = Manifest::of_folder_via(dir.path(), &Stop::default(), &mut |_so_far| {
            std::fs::remove_dir_all(dir.path().join("sub")).expect("fixture");
        })
        .expect("a folder that vanishes is not a failure");

        assert_eq!(manifest.unread, vec!["sub".to_string()]);
        assert!(manifest.entries.contains_key("a.txt"));
        assert!(!manifest.entries.contains_key("sub"));
    }

    /// A folder the manifest may not look inside is no folder that vanished.
    #[cfg(unix)]
    #[test]
    fn a_subfolder_that_refuses_permission_fails_the_manifest() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workshop("manifest-refused");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("fixture");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("fixture");
        // Root lists a `0000` directory; the case binds only where the
        // permission does.
        let refused = std::fs::read_dir(&locked).is_err();
        let result = Manifest::of_folder(dir.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("fixture");
        assert!(!refused || result.is_err());
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
