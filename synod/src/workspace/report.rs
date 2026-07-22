//! The after-run report and the undo entry points.
//!
//! These are the calls the GUI makes: [`job_report`] for "what did the
//! job change", [`undo_file`] and [`undo_all`] to put things back.  The
//! headless run prints [`render`]'s text; the GUI consumes the types.

use crate::workspace::changes::{Change, ChangeSet};
use crate::workspace::history::HistoryStore;
use crate::workspace::manifest::Manifest;
use crate::workspace::restore::{Resolution, RestoreOutcome, restore};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What the folder's most recent job changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobReport {
    /// The granted folder, as a host path.
    pub folder: String,
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
    let Some((before, after)) = store.latest_job()? else {
        return Err(format!(
            "Synod has not finished a job in {} yet, so there is nothing to report.",
            folder.display()
        ));
    };
    let (finished_at_ms, now) = match after {
        Some(checkpoint) => (Some(checkpoint.taken_at_ms), checkpoint.manifest),
        None => (None, Manifest::of_folder(folder)?),
    };
    Ok(JobReport {
        folder: folder.to_string_lossy().into_owned(),
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
    let (before, after) = last_job(store, folder)?;
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
    let (before, after) = last_job(store, folder)?;
    // A rename is one change with two names.  Undoing it must put the old
    // name back AND take the new one away, so either name resolves to the
    // pair — otherwise half the rename would stand.
    let now = match &after {
        Some(checkpoint) => checkpoint.manifest.clone(),
        None => Manifest::of_folder(folder)?,
    };
    let names = ChangeSet::between(&before.manifest, &now)
        .changes
        .iter()
        .find_map(|change| match change {
            Change::Renamed { from, to } if from == path || to == path => {
                Some(vec![from.clone(), to.clone()])
            }
            _ => None,
        })
        .unwrap_or_else(|| vec![path.to_string()]);
    restore(
        store,
        folder,
        &before,
        after.as_ref(),
        Some(&names),
        resolution,
    )
}

/// The report as plain text, for the headless run's output.
pub fn render(report: &JobReport) -> String {
    if report.changes.is_empty() {
        return format!("The assistant changed nothing in {}.\n", report.folder);
    }
    let mut out = format!("What changed in {}:\n", report.folder);
    for change in &report.changes.changes {
        out.push_str("  ");
        out.push_str(&change.describe());
        out.push('\n');
    }
    out.push_str(
        "\nEvery change above can be put back the way it was — \
         one file at a time or all at once — from this folder's history in Synod.\n",
    );
    out
}

fn last_job(
    store: &HistoryStore,
    folder: &Path,
) -> Result<
    (
        crate::workspace::history::Checkpoint,
        Option<crate::workspace::history::Checkpoint>,
    ),
    String,
> {
    store.latest_job()?.ok_or_else(|| {
        format!(
            "Synod has not run a job in {} yet, so there is nothing to undo.",
            folder.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::history::Moment;
    use std::path::PathBuf;

    /// A workshop holding a granted folder and a store, side by side.
    fn workshop(tag: &str) -> (PathBuf, PathBuf, HistoryStore) {
        let dir = std::env::temp_dir().join(format!("synod-report-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = dir.join("Admissions");
        std::fs::create_dir_all(&folder).expect("temp workshop");
        let store = HistoryStore::open_at(&dir.join("history")).expect("store opens");
        let dir = std::fs::canonicalize(&dir).expect("temp workshop resolves");
        (dir.clone(), dir.join("Admissions"), store)
    }

    #[test]
    fn the_report_names_each_change_and_the_way_back() {
        let (dir, folder, store) = workshop("report");
        std::fs::write(folder.join("invoices-2026.xlsx"), b"v1").expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        std::fs::write(folder.join("invoices-2026.xlsx"), b"v2").expect("job");
        std::fs::create_dir(folder.join("reminder-letters")).expect("job");
        store.capture(&folder, Moment::After).expect("after");

        let report = job_report(&store, &folder).expect("reports");
        assert!(report.finished_at_ms.is_some());
        let text = render(&report);
        assert!(text.contains("Modified invoices-2026.xlsx"), "{text}");
        assert!(text.contains("Created reminder-letters/"), "{text}");
        assert!(
            text.contains("put back"),
            "the way back must be said: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_quiet_job_says_so() {
        let (dir, folder, store) = workshop("quiet");
        std::fs::write(folder.join("a.txt"), b"same").expect("fixture");
        store.capture(&folder, Moment::Before).expect("baseline");
        store.capture(&folder, Moment::After).expect("after");

        let report = job_report(&store, &folder).expect("reports");
        assert!(report.changes.is_empty());
        assert!(render(&report).contains("changed nothing"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn before_any_job_there_is_nothing_to_report_or_undo() {
        let (dir, folder, store) = workshop("nothing");
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
        let (dir, folder, store) = workshop("rename");
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

    #[test]
    fn undo_all_and_undo_file_reach_the_driver() {
        let (dir, folder, store) = workshop("undo");
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
