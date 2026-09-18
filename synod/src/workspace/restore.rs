//! Putting things back: the conflict-checked restore driver.
//!
//! The law here: nothing is ever silently overwritten, and nothing is
//! ever destroyed.  A path whose bytes differ from what the job left
//! behind was edited afterwards — that is a conflict the caller
//! resolves, never a default.  And before a restore touches anything, the
//! folder as it stands is checkpointed, so every byte an undo replaces or
//! removes stays recoverable.
//!
//! # On the filesystem calls below
//!
//! Restoring a folder is the safety net itself: writing back kept bytes,
//! removing what should not exist, and the tests that simulate a job by
//! writing and rewriting fixture files. It runs only when the user asks to
//! undo, never as the model's turn-time output, so none of it raises a
//! card.
#![allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: the restore driver puts files back at the user's own \
              request; it is not the model's turn-time I/O. See the module docs."
)]

use crate::workspace::history::{Checkpoint, HistoryStore, Moment};
use crate::workspace::manifest::EntryKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// What to do with a path that changed after the job finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Leave the newer version alone and report it as a conflict.
    KeepCurrent,
    /// Put the older version back anyway.  The newer bytes are kept in
    /// the store first, so nothing is lost.
    PutBack,
}

/// What a restore did.
///
/// A restore never aborts partway and throws its record away: every path
/// it has already put back, removed, or set aside stays on this outcome
/// even when a later path in the same run cannot be handled.  That is why
/// [`restore`] returns `Ok` far more often than the shape of the code
/// might suggest — a path this platform cannot recreate, or one that hit
/// a genuine I/O error, is recorded here rather than raised, so the
/// caller always sees the whole picture of what changed on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOutcome {
    /// Paths put back to their checkpointed version.
    pub put_back: Vec<String>,
    /// Paths removed because they did not exist at the checkpoint.
    pub removed: Vec<String>,
    /// Paths left alone under [`Resolution::KeepCurrent`].
    pub conflicts: Vec<String>,
    /// Paths whose checkpointed entry this platform cannot recreate — a
    /// symlink baked into the manifest on a machine that makes no
    /// symlinks Synod can write, for instance.  Left exactly as they
    /// stood; the restore carries on with everything else rather than
    /// abandoning the whole run over one link.
    pub unrestorable: Vec<String>,
    /// Plain sentences describing a path the restore tried to put back
    /// or remove and could not, for a reason that is not the platform's
    /// fault — a permission error, a file locked by another process, and
    /// so on.  Recorded rather than returned as an early `Err` so that
    /// every path handled before the failure is not thrown away with it.
    pub failed: Vec<String>,
}

/// Whether two records of one path describe the same thing, absent record
/// and all — [`EntryKind::same_as`] lifted over `Option`, so a timestamp
/// that moved is neither a change to undo nor an edit to conflict on.
fn same(a: Option<&EntryKind>, b: Option<&EntryKind>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same_as(b),
        (None, None) => true,
        _ => false,
    }
}

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
/// A plain sentence when `only` names nothing known, or when checkpointing
/// the folder before touching anything fails — nothing has been written
/// or removed yet at that point, so there is no partial outcome to lose.
/// Once the put-back loop itself starts, nothing it meets aborts it: a
/// path this platform cannot recreate lands in
/// [`RestoreOutcome::unrestorable`], and a genuine I/O failure lands in
/// [`RestoreOutcome::failed`], with the restore continuing to the
/// remaining paths either way. That keeps the one promise this module
/// cannot break — the record of what was already put back or removed
/// never gets discarded behind a `?`.
pub(crate) fn restore(
    store: &HistoryStore,
    root: &Path,
    baseline: &Checkpoint,
    run_left: Option<&Checkpoint>,
    only: Option<&[String]>,
    resolution: Resolution,
) -> Result<RestoreOutcome, String> {
    // The safety net: everything about to be touched is kept first.
    let current = store.capture(root, Moment::Undo)?;

    let selected = |path: &str| only.is_none_or(|names| names.iter().any(|one| covers(path, one)));
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
        if same(want, now) {
            continue;
        }
        let edited_after =
            run_left.is_none_or(|left| !same(left.manifest.entries.get(path), now));
        if edited_after && resolution == Resolution::KeepCurrent {
            outcome.conflicts.push(path.clone());
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
    for path in to_remove.iter().rev() {
        let target = root.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            outcome.removed.push((*path).clone());
            continue;
        };
        if meta.file_type().is_dir() {
            if std::fs::remove_dir(&target).is_err() {
                outcome.conflicts.push((*path).clone());
                continue;
            }
        } else if let Err(e) = std::fs::remove_file(&target) {
            outcome
                .failed
                .push(format!("Synod could not remove {}: {e}.", target.display()));
            continue;
        }
        outcome.removed.push((*path).clone());
    }

    // Shallowest first, so parents exist before their contents.
    for (path, kind) in to_put {
        let target = root.join(path);
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            match (kind, meta.file_type().is_dir()) {
                (EntryKind::Folder, true) => {}
                (_, true) => {
                    if std::fs::remove_dir(&target).is_err() {
                        outcome.conflicts.push(path.clone());
                        continue;
                    }
                }
                (_, false) => {
                    if let Err(e) = std::fs::remove_file(&target) {
                        outcome.failed.push(format!(
                            "Synod could not put back {}: {e}.",
                            target.display()
                        ));
                        continue;
                    }
                }
            }
        }
        match kind {
            EntryKind::Folder => {
                if let Err(e) = std::fs::create_dir_all(&target) {
                    outcome.failed.push(format!(
                        "Synod could not put back {}: {e}.",
                        target.display()
                    ));
                    continue;
                }
            }
            EntryKind::File { hash, mode, .. } => {
                if let Err(e) = store.place(hash, *mode, &target) {
                    outcome.failed.push(e);
                    continue;
                }
            }
            EntryKind::Link { target: text } => {
                if let Some(parent) = target.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    outcome.failed.push(format!(
                        "Synod could not put back {}: {e}.",
                        target.display()
                    ));
                    continue;
                }
                #[cfg(unix)]
                if let Err(e) = std::os::unix::fs::symlink(text, &target) {
                    outcome.failed.push(format!(
                        "Synod could not put back {}: {e}.",
                        target.display()
                    ));
                    continue;
                }
                // A link this platform cannot recreate is not an error to
                // abort the run over — it is the exact "leave it alone and
                // report it" shape `Resolution::KeepCurrent` already
                // models, just forced by the platform rather than chosen
                // by the caller. Every other path still gets put back.
                #[cfg(not(unix))]
                {
                    let _ = text;
                    outcome.unrestorable.push(path.clone());
                    continue;
                }
            }
        }
        outcome.put_back.push(path.clone());
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::granted_workshop as workshop;
    use crate::workspace::manifest::{ContentHash, Manifest};

    /// A whole-job undo puts modifications, deletions, and creations all
    /// back, and says so.
    #[test]
    fn a_full_undo_returns_the_folder_to_its_baseline() {
        let (_dir, folder, store) = workshop("restore-full");
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
            without_mtimes(Manifest::of_folder(&folder).expect("rereads")),
            without_mtimes(baseline.manifest),
            "the folder must match its baseline exactly, bytes and mode; a restore \
             writes fresh bytes and so a fresh mtime, which is not part of its promise"
        );
    }

    /// A restore's promise is bytes and mode; the stat clock a quick
    /// capture leans on is not part of it, so a comparison of "the folder
    /// matches its baseline" must look past mtime.
    fn without_mtimes(mut manifest: Manifest) -> Manifest {
        for kind in manifest.entries.values_mut() {
            if let EntryKind::File { mtime_ns, .. } = kind {
                *mtime_ns = 0;
            }
        }
        manifest
    }

    /// The core law: a file edited after the job is never silently
    /// overwritten, and forcing it keeps the newer bytes first.
    #[test]
    fn an_edit_after_the_job_is_a_conflict_and_never_destroyed() {
        let (_dir, folder, store) = workshop("restore-conflict");
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
        assert_eq!(outcome.conflicts, ["report.txt"]);
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
    }

    /// Without the job's closing checkpoint nothing can be told apart
    /// from a later edit, so everything differing conflicts.
    #[test]
    fn a_crashed_run_makes_every_difference_a_conflict() {
        let (_dir, folder, store) = workshop("restore-crashed");
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
    }

    /// A one-file undo touches that file and nothing else.
    #[test]
    fn undoing_one_file_leaves_the_rest_of_the_job_standing() {
        let (_dir, folder, store) = workshop("restore-one-file");
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
    }

    /// A link this platform cannot recreate is recorded and skipped, not
    /// raised — the restore still puts back every other path from the same
    /// job. The baseline's manifest is just data, so a `Link` entry stands
    /// in for a real symlink without this test needing to make one.
    #[test]
    fn an_unrestorable_link_does_not_stop_the_rest_of_the_restore() {
        let (_dir, folder, store) = workshop("restore-unrestorable-link");
        std::fs::write(folder.join("plain.txt"), b"original").expect("fixture");
        let mut baseline = store.capture(&folder, Moment::Before).expect("baseline");
        baseline.manifest.entries.insert(
            "link.txt".to_string(),
            EntryKind::Link {
                target: "plain.txt".to_string(),
            },
        );

        std::fs::write(folder.join("plain.txt"), b"rewritten").expect("job");
        let after = store.capture(&folder, Moment::After).expect("after");

        let outcome = restore(
            &store,
            &folder,
            &baseline,
            Some(&after),
            None,
            Resolution::KeepCurrent,
        )
        .expect("an unrestorable link must not abort the restore");

        assert!(
            outcome.put_back.contains(&"plain.txt".to_string()),
            "the rest of the job must still be put back: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(folder.join("plain.txt")).expect("rereads"),
            b"original"
        );

        #[cfg(not(unix))]
        {
            assert_eq!(outcome.unrestorable, ["link.txt"]);
            assert!(
                outcome.conflicts.is_empty() && outcome.failed.is_empty(),
                "a platform limit is neither a conflict nor a failure: {outcome:?}"
            );
        }
        #[cfg(unix)]
        {
            assert!(outcome.unrestorable.is_empty());
            assert!(outcome.put_back.contains(&"link.txt".to_string()));
        }
    }

    /// A genuine I/O failure on one path — as opposed to a conflict, or a
    /// link this platform cannot make — must not throw away the record of
    /// every other path the same restore already put back.  Locking a file
    /// so its put-back is refused stands in for the disk errors this
    /// module cannot provoke on demand (a full disk, a vanished mount).
    /// Windows-only: the exclusive-share handle it uses to force the
    /// failure is a Windows-specific lever, and this crate's target here
    /// is Windows.
    #[test]
    #[cfg(windows)]
    fn a_failed_put_back_does_not_erase_what_already_worked() {
        let (_dir, folder, store) = workshop("restore-failed-keeps-outcome");
        std::fs::write(folder.join("a.txt"), b"a original").expect("fixture");
        std::fs::write(folder.join("locked.txt"), b"locked original").expect("fixture");
        let baseline = store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::write(folder.join("a.txt"), b"a rewritten").expect("job");
        std::fs::write(folder.join("locked.txt"), b"locked rewritten").expect("job");
        let after = store.capture(&folder, Moment::After).expect("after");

        // An exclusive-share handle held open makes Windows refuse the
        // put-back's own remove_file with "used by another process" — a
        // stand-in for the disk errors this test cannot otherwise provoke
        // on demand. Dropped before the assertions so the tempdir guard
        // can tear the folder down afterwards.
        let locked = folder.join("locked.txt");
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&locked)
                .expect("lock it")
        };

        let outcome = restore(
            &store,
            &folder,
            &baseline,
            Some(&after),
            None,
            Resolution::KeepCurrent,
        )
        .expect("a mid-loop I/O failure is recorded on the outcome, not raised as an Err");

        drop(handle);

        assert_eq!(
            outcome.put_back,
            ["a.txt"],
            "the path that worked must still be reported"
        );
        assert_eq!(
            outcome.failed.len(),
            1,
            "the locked path's failure must be recorded, not silently dropped: {outcome:?}"
        );
        assert!(
            outcome.failed[0].contains("locked.txt"),
            "the failure must name the path it happened to: {}",
            outcome.failed[0]
        );
        assert_eq!(
            std::fs::read(folder.join("a.txt")).expect("rereads"),
            b"a original",
            "a's put-back must not be lost because locked.txt's failed"
        );
    }

    #[test]
    fn a_name_nobody_has_ever_seen_is_refused_plainly() {
        let (_dir, folder, store) = workshop("restore-unknown");
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
    }
}
