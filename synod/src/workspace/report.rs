//! The after-run report and the undo entry points.
//!
//! These are the calls the GUI makes: [`job_report`] for "what did the
//! job change", [`undo_file`] and [`undo_all`] to put things back.

use crate::workspace::changes::{Change, ChangeSet};
use crate::workspace::history::{Checkpoint, HistoryStore};
use crate::workspace::manifest::Manifest;
use crate::workspace::restore::{Resolution, RestoreOutcome, covers, restore};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What the folder's most recent job changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobReport {
    /// When the job's closing checkpoint was taken; absent when the run
    /// died before taking it.
    pub finished_at_ms: Option<u64>,
    pub changes: ChangeSet,
}

/// The report for `folder`'s most recent job.
///
/// A run that died before its closing checkpoint is reported against the
/// folder as it stands now instead.
///
/// # Errors
/// A plain sentence when no job has run here, or the records fail to read.
pub fn job_report(store: &HistoryStore, folder: &Path) -> Result<JobReport, String> {
    let (before, after) = last_job(store, folder, "report")?;
    let (finished_at_ms, now) = match after {
        Some(checkpoint) => (Some(checkpoint.taken_at_ms), checkpoint.manifest),
        None => (None, Manifest::of_folder(folder)?),
    };
    Ok(JobReport {
        finished_at_ms,
        changes: ChangeSet::between(&before.manifest, &now),
    })
}

/// Put everything in `folder` back as it was before the last job.
///
/// # Errors
/// A plain sentence when no job has run here, or the restore fails partway.
pub fn undo_all(
    store: &HistoryStore,
    folder: &Path,
    resolution: Resolution,
) -> Result<RestoreOutcome, String> {
    let (before, after) = last_job(store, folder, "undo")?;
    restore(store, folder, &before, after.as_ref(), None, resolution)
}

/// Put one file — or one folder and everything under it — back as it was
/// before the last job.  `path` is named as the report names it.
///
/// # Errors
/// A plain sentence when no job has run here, when `path` names nothing
/// known, or when the restore fails partway.
pub fn undo_file(
    store: &HistoryStore,
    folder: &Path,
    path: &str,
    resolution: Resolution,
) -> Result<RestoreOutcome, String> {
    let (before, after) = last_job(store, folder, "undo")?;
    let now = match &after {
        Some(checkpoint) => checkpoint.manifest.clone(),
        None => Manifest::of_folder(folder)?,
    };
    let names = rename_closure(path, &ChangeSet::between(&before.manifest, &now));
    restore(
        store,
        folder,
        &before,
        after.as_ref(),
        Some(&names),
        resolution,
    )
}

/// The names `path` resolves to, expanded to a fixpoint over `changes`'
/// renames.  A rename is one change with two names; undoing it must put
/// the old name back AND take the new one away, so a name covering
/// either side — exactly, or as a folder over a file moved into or out of
/// it — pulls in the other side too, or half the rename would stand.  The
/// loop, not one pass, matters: an endpoint just pulled in can itself
/// cover another rename's endpoint.
fn rename_closure(path: &str, changes: &ChangeSet) -> Vec<String> {
    let mut names = vec![path.to_string()];
    loop {
        let mut grew = false;
        for change in &changes.changes {
            let Change::Renamed { from, to } = change else {
                continue;
            };
            if !names.iter().any(|n| covers(from, n) || covers(to, n)) {
                continue;
            }
            for endpoint in [from, to] {
                if !names.contains(endpoint) {
                    names.push(endpoint.clone());
                    grew = true;
                }
            }
        }
        if !grew {
            return names;
        }
    }
}

fn last_job(
    store: &HistoryStore,
    folder: &Path,
    doing: &str,
) -> Result<(Checkpoint, Option<Checkpoint>), String> {
    store.latest_job()?.ok_or_else(|| {
        format!(
            "Synod has not run a job in {} yet, so there is nothing to {doing}.",
            folder.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::granted_workshop as workshop;
    use crate::workspace::history::Moment;

    #[test]
    fn the_report_names_each_change() {
        let (dir, folder, store) = workshop("report-report");
        std::fs::write(folder.join("invoices-2026.xlsx"), b"v1").expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::write(folder.join("invoices-2026.xlsx"), b"v2").expect("job");
        std::fs::create_dir(folder.join("reminder-letters")).expect("job");
        store.capture(&folder, Moment::After).expect("after");

        let report = job_report(&store, &folder).expect("reports");
        assert!(report.finished_at_ms.is_some());
        assert!(report.changes.changes.contains(&Change::Modified {
            path: "invoices-2026.xlsx".into(),
        }));
        assert!(report.changes.changes.contains(&Change::Created {
            path: "reminder-letters".into(),
            folder: true,
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_quiet_job_says_so() {
        let (dir, folder, store) = workshop("report-quiet");
        std::fs::write(folder.join("a.txt"), b"same").expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        store.capture(&folder, Moment::After).expect("after");

        let report = job_report(&store, &folder).expect("reports");
        assert!(report.changes.changes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn before_any_job_there_is_nothing_to_report_or_undo() {
        let (dir, folder, store) = workshop("report-nothing");
        let message = job_report(&store, &folder).expect_err("no job yet");
        assert!(message.contains("nothing to report"), "{message}");
        let message = undo_all(&store, &folder, Resolution::KeepCurrent).expect_err("no job yet");
        assert!(message.contains("nothing to undo"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rename undone by either of its names undoes both sides: the old
    /// name comes back and the new one goes away — never half a rename.
    #[test]
    fn undoing_a_rename_by_either_name_undoes_both_sides() {
        let (dir, folder, store) = workshop("report-rename");
        std::fs::write(folder.join("scan001.pdf"), b"the scan").expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::rename(folder.join("scan001.pdf"), folder.join("enrolment.pdf")).expect("job");
        store.capture(&folder, Moment::After).expect("after");

        let outcome = undo_file(&store, &folder, "enrolment.pdf", Resolution::KeepCurrent)
            .expect("a rename undoes by its new name");
        assert_eq!(outcome.put_back, ["scan001.pdf"]);
        assert_eq!(outcome.removed, ["enrolment.pdf"]);
        assert!(folder.join("scan001.pdf").exists());
        assert!(!folder.join("enrolment.pdf").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Undoing the folder a file was renamed *into* must not strand the
    /// file: both the folder's own creation and the rename that landed a
    /// file inside it come back together, so the old name reappears
    /// instead of the file simply vanishing with the folder.
    #[test]
    fn undoing_a_created_folder_also_undoes_a_rename_into_it() {
        let (dir, folder, store) = workshop("report-rename-into-folder");
        std::fs::write(folder.join("draft.docx"), b"the draft").expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::create_dir(folder.join("reports")).expect("job");
        std::fs::rename(
            folder.join("draft.docx"),
            folder.join("reports").join("draft.docx"),
        )
        .expect("job");
        store.capture(&folder, Moment::After).expect("after");

        let outcome = undo_file(&store, &folder, "reports", Resolution::KeepCurrent)
            .expect("undoing the folder undoes the rename into it too");
        assert_eq!(outcome.put_back, ["draft.docx"]);
        assert_eq!(outcome.removed, ["reports/draft.docx", "reports"]);
        assert!(folder.join("draft.docx").exists());
        assert!(!folder.join("reports").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mirror case: undoing a deleted folder a file was renamed *out
    /// of* must also undo that rename, or the file ends up duplicated —
    /// once under the restored folder, once at the name it was moved to.
    #[test]
    fn undoing_a_deleted_folder_also_undoes_a_rename_out_of_it() {
        let (dir, folder, store) = workshop("report-rename-out-of-folder");
        std::fs::create_dir(folder.join("archive")).expect("fixture");
        std::fs::write(folder.join("archive").join("report.docx"), b"the report")
            .expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::rename(
            folder.join("archive").join("report.docx"),
            folder.join("report.docx"),
        )
        .expect("job");
        std::fs::remove_dir(folder.join("archive")).expect("job");
        store.capture(&folder, Moment::After).expect("after");

        let outcome = undo_file(&store, &folder, "archive", Resolution::KeepCurrent)
            .expect("undoing the folder undoes the rename out of it too");
        assert_eq!(outcome.put_back, ["archive", "archive/report.docx"]);
        assert_eq!(outcome.removed, ["report.docx"]);
        assert!(folder.join("archive").join("report.docx").exists());
        assert!(!folder.join("report.docx").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_all_and_undo_file_reach_the_driver() {
        let (dir, folder, store) = workshop("report-undo");
        std::fs::write(folder.join("a.txt"), b"a original").expect("fixture");
        std::fs::write(folder.join("b.txt"), b"b original").expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::write(folder.join("a.txt"), b"a rewritten").expect("job");
        std::fs::write(folder.join("b.txt"), b"b rewritten").expect("job");
        store.capture(&folder, Moment::After).expect("after");

        let outcome =
            undo_file(&store, &folder, "a.txt", Resolution::KeepCurrent).expect("one file undoes");
        assert_eq!(outcome.put_back, ["a.txt"]);
        let outcome = undo_all(&store, &folder, Resolution::KeepCurrent).expect("the rest undoes");
        assert_eq!(outcome.put_back, ["b.txt"]);
        assert_eq!(
            std::fs::read(folder.join("a.txt")).expect("rereads"),
            b"a original"
        );
        assert_eq!(
            std::fs::read(folder.join("b.txt")).expect("rereads"),
            b"b original"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
