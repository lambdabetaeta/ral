//! Putting things back: the conflict-checked restore driver.
//!
//! The law here: nothing is ever silently overwritten, and nothing is
//! ever destroyed.  A path whose bytes differ from what the job left
//! behind was edited afterwards — that is a [`Conflict`] the caller
//! resolves, never a default.  And before a restore touches anything, the
//! folder as it stands is checkpointed, so every byte an undo replaces or
//! removes stays recoverable.

use crate::workspace::history::{Checkpoint, HistoryStore, Moment};
use crate::workspace::manifest::EntryKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// What to do with a path that changed after the job finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Leave the newer version alone and report it as a [`Conflict`].
    KeepCurrent,
    /// Put the older version back anyway.  The newer bytes are kept in
    /// the store first, so nothing is lost.
    PutBack,
}

/// A path left alone because it changed after the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
}

/// What a restore did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOutcome {
    /// Paths put back to their checkpointed version.
    pub put_back: Vec<String>,
    /// Paths removed because they did not exist at the checkpoint.
    pub removed: Vec<String>,
    /// Paths left alone under [`Resolution::KeepCurrent`].
    pub conflicts: Vec<Conflict>,
}

/// Put `root` back to `baseline`.
///
/// `run_left` is the job's closing checkpoint; a path that now differs
/// from it was edited after the job and conflicts.  When `run_left` is
/// absent — the run died before taking it — nothing can be told apart
/// from a later edit, so every differing path conflicts.
///
/// `only` limits the restore to the named paths — each named as the
/// report names it, and each covering a folder and everything under it.
/// It is a set rather than one name because one *change* can span two
/// names: a rename is undone by putting the old name back and taking the
/// new one away, and [`undo_file`](crate::workspace::report::undo_file)
/// resolves either name to the pair.
///
/// # Errors
/// A plain sentence when `only` names nothing known, or when reading or
/// writing the folder fails partway.
pub fn restore(
    store: &HistoryStore,
    root: &Path,
    baseline: &Checkpoint,
    run_left: Option<&Checkpoint>,
    only: Option<&[String]>,
    resolution: Resolution,
) -> Result<RestoreOutcome, String> {
    // The safety net: everything about to be touched is kept first.
    let current = store.capture(root, Moment::Undo)?;

    let selected = |path: &str| {
        only.is_none_or(|names| {
            names.iter().any(|one| {
                path == one
                    || path
                        .strip_prefix(one.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
    };
    if let Some(names) = only {
        let known = baseline.manifest.entries.keys().any(|p| selected(p))
            || current.manifest.entries.keys().any(|p| selected(p));
        if !known {
            return Err(format!(
                "There is nothing called {} in the folder now, and there was nothing \
                 by that name before the job either. Names are written as the report \
                 shows them, for example letters/offer.docx.",
                names.join(", ")
            ));
        }
    }

    let mut outcome = RestoreOutcome::default();
    let mut to_remove: Vec<&String> = Vec::new();
    let mut to_put: Vec<(&String, &EntryKind)> = Vec::new();
    let paths: BTreeSet<&String> = baseline
        .manifest
        .entries
        .keys()
        .chain(current.manifest.entries.keys())
        .collect();
    for path in paths {
        if !selected(path) {
            continue;
        }
        let want = baseline.manifest.entries.get(path);
        let now = current.manifest.entries.get(path);
        if want == now {
            continue;
        }
        let edited_after = run_left.is_none_or(|left| left.manifest.entries.get(path) != now);
        if edited_after && resolution == Resolution::KeepCurrent {
            outcome.conflicts.push(Conflict { path: path.clone() });
            continue;
        }
        match want {
            Some(kind) => to_put.push((path, kind)),
            None => to_remove.push(path),
        }
    }

    // Deepest first, so folders are empty by the time their turn comes.
    // A folder that will not empty holds something kept above — a
    // conflict, not an error.
    to_remove.sort();
    for path in to_remove.iter().rev() {
        let target = root.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            outcome.removed.push((*path).clone());
            continue;
        };
        if meta.file_type().is_dir() {
            if std::fs::remove_dir(&target).is_err() {
                outcome.conflicts.push(Conflict {
                    path: (*path).clone(),
                });
                continue;
            }
        } else {
            std::fs::remove_file(&target)
                .map_err(|e| format!("Synod could not remove {}: {e}.", target.display()))?;
        }
        outcome.removed.push((*path).clone());
    }

    // Shallowest first, so parents exist before their contents.
    to_put.sort_by(|a, b| a.0.cmp(b.0));
    for (path, kind) in to_put {
        let target = root.join(path);
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            match (kind, meta.file_type().is_dir()) {
                (EntryKind::Folder, true) => {}
                (_, true) => {
                    if std::fs::remove_dir(&target).is_err() {
                        outcome.conflicts.push(Conflict { path: path.clone() });
                        continue;
                    }
                }
                (_, false) => {
                    std::fs::remove_file(&target).map_err(|e| {
                        format!("Synod could not put back {}: {e}.", target.display())
                    })?;
                }
            }
        }
        match kind {
            EntryKind::Folder => {
                std::fs::create_dir_all(&target)
                    .map_err(|e| format!("Synod could not put back {}: {e}.", target.display()))?;
            }
            EntryKind::File { hash, .. } => store.place(hash, &target)?,
            EntryKind::Link { target: text } => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        format!("Synod could not put back {}: {e}.", target.display())
                    })?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(text, &target)
                    .map_err(|e| format!("Synod could not put back {}: {e}.", target.display()))?;
                #[cfg(not(unix))]
                return Err(format!(
                    "Synod cannot put back the link {} on this computer.",
                    target.display()
                ));
            }
        }
        outcome.put_back.push(path.clone());
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::manifest::{ContentHash, Manifest};
    use std::path::PathBuf;

    /// A workshop holding a granted folder and a store, side by side.
    fn workshop(tag: &str) -> (PathBuf, PathBuf, HistoryStore) {
        let dir = std::env::temp_dir().join(format!("synod-restore-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = dir.join("Admissions");
        std::fs::create_dir_all(&folder).expect("temp workshop");
        let store = HistoryStore::open_at(&dir.join("history")).expect("store opens");
        let dir = std::fs::canonicalize(&dir).expect("temp workshop resolves");
        (dir.clone(), dir.join("Admissions"), store)
    }

    /// A whole-job undo puts modifications, deletions, and creations all
    /// back, and says so.
    #[test]
    fn a_full_undo_returns_the_folder_to_its_baseline() {
        let (dir, folder, store) = workshop("full");
        std::fs::write(folder.join("edited.txt"), b"original").expect("fixture");
        std::fs::write(folder.join("deleted.txt"), b"kept safe").expect("fixture");
        std::fs::create_dir(folder.join("emptied")).expect("fixture");
        std::fs::write(folder.join("emptied").join("inner.txt"), b"inner").expect("fixture");
        let baseline = store.capture(&folder, Moment::Before).expect("baseline");

        // The job: edit, delete, create a file and a folder.
        std::fs::write(folder.join("edited.txt"), b"rewritten").expect("job");
        std::fs::remove_file(folder.join("deleted.txt")).expect("job");
        std::fs::remove_file(folder.join("emptied").join("inner.txt")).expect("job");
        std::fs::create_dir(folder.join("made")).expect("job");
        std::fs::write(folder.join("made").join("new.txt"), b"fresh").expect("job");
        let after = store.capture(&folder, Moment::After).expect("after");

        let outcome = restore(
            &store,
            &folder,
            &baseline,
            Some(&after),
            None,
            Resolution::KeepCurrent,
        )
        .expect("restores");
        assert!(
            outcome.conflicts.is_empty(),
            "nothing was edited after the job"
        );
        assert_eq!(
            outcome.put_back,
            ["deleted.txt", "edited.txt", "emptied/inner.txt"]
        );
        assert_eq!(outcome.removed, ["made/new.txt", "made"]);
        assert_eq!(
            Manifest::of_folder(&folder).expect("rereads"),
            baseline.manifest,
            "the folder must match its baseline exactly"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The core law: a file edited after the job is never silently
    /// overwritten, and forcing it keeps the newer bytes first.
    #[test]
    fn an_edit_after_the_job_is_a_conflict_and_never_destroyed() {
        let (dir, folder, store) = workshop("conflict");
        std::fs::write(folder.join("report.txt"), b"original").expect("fixture");
        let baseline = store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::write(folder.join("report.txt"), b"the job's version").expect("job");
        let after = store.capture(&folder, Moment::After).expect("after");

        // The user edits after the job finished.
        std::fs::write(folder.join("report.txt"), b"my later edit").expect("user edit");

        let outcome = restore(
            &store,
            &folder,
            &baseline,
            Some(&after),
            None,
            Resolution::KeepCurrent,
        )
        .expect("restores");
        assert_eq!(
            outcome.conflicts,
            vec![Conflict {
                path: "report.txt".into(),
            }]
        );
        assert_eq!(
            std::fs::read(folder.join("report.txt")).expect("rereads"),
            b"my later edit",
            "a conflicted file must be left alone"
        );

        let outcome = restore(
            &store,
            &folder,
            &baseline,
            Some(&after),
            None,
            Resolution::PutBack,
        )
        .expect("restores");
        assert_eq!(outcome.put_back, ["report.txt"]);
        assert_eq!(
            std::fs::read(folder.join("report.txt")).expect("rereads"),
            b"original"
        );
        assert_eq!(
            store
                .read_object(&ContentHash::of_bytes(b"my later edit"))
                .expect("the overwritten edit was kept first"),
            b"my later edit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without the job's closing checkpoint nothing can be told apart
    /// from a later edit, so everything differing conflicts.
    #[test]
    fn a_crashed_run_makes_every_difference_a_conflict() {
        let (dir, folder, store) = workshop("crashed");
        std::fs::write(folder.join("a.txt"), b"original").expect("fixture");
        let baseline = store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::write(folder.join("a.txt"), b"changed by someone").expect("job");

        let outcome = restore(
            &store,
            &folder,
            &baseline,
            None,
            None,
            Resolution::KeepCurrent,
        )
        .expect("restores");
        assert_eq!(outcome.conflicts.len(), 1);
        assert!(outcome.put_back.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A one-file undo touches that file and nothing else.
    #[test]
    fn undoing_one_file_leaves_the_rest_of_the_job_standing() {
        let (dir, folder, store) = workshop("one-file");
        std::fs::write(folder.join("a.txt"), b"a original").expect("fixture");
        std::fs::write(folder.join("b.txt"), b"b original").expect("fixture");
        let baseline = store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::write(folder.join("a.txt"), b"a rewritten").expect("job");
        std::fs::write(folder.join("b.txt"), b"b rewritten").expect("job");
        let after = store.capture(&folder, Moment::After).expect("after");

        let outcome = restore(
            &store,
            &folder,
            &baseline,
            Some(&after),
            Some(&["a.txt".to_string()][..]),
            Resolution::KeepCurrent,
        )
        .expect("restores");
        assert_eq!(outcome.put_back, ["a.txt"]);
        assert_eq!(
            std::fs::read(folder.join("a.txt")).expect("rereads"),
            b"a original"
        );
        assert_eq!(
            std::fs::read(folder.join("b.txt")).expect("rereads"),
            b"b rewritten",
            "the other file keeps the job's version"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_nobody_has_ever_seen_is_refused_plainly() {
        let (dir, folder, store) = workshop("unknown");
        std::fs::write(folder.join("a.txt"), b"x").expect("fixture");
        let baseline = store.capture(&folder, Moment::Before).expect("baseline");
        let after = store.capture(&folder, Moment::After).expect("after");

        let message = restore(
            &store,
            &folder,
            &baseline,
            Some(&after),
            Some(&["no-such-file.txt".to_string()][..]),
            Resolution::KeepCurrent,
        )
        .expect_err("an unknown name must be refused");
        assert!(
            message.contains("nothing called no-such-file.txt"),
            "the refusal must name the file: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
