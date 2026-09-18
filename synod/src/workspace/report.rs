//! The after-run report: what the job changed, and what it could not see.
//!
//! One call the whole window rests on — [`job_report`], a pure function of
//! the two manifests the conversation already holds.  It opens nothing,
//! reads nothing, and touches no file: by the time it runs, both walks are
//! done and the folder has already told it everything it is going to.

use crate::workspace::changes::ChangeSet;
use crate::workspace::manifest::{Manifest, merge_unread};
use serde::{Deserialize, Serialize};

/// What a job changed, judged between the folder as it stood before and
/// the folder as it stands now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobReport {
    /// When the closing walk finished — the instant this report was made,
    /// which is the same thing: nothing is read after it.
    pub finished_at_ms: u64,
    pub changes: ChangeSet,
    /// Paths one of the two walks could not read, so nothing about them is
    /// shown as a change.  Named rather than quietly dropped: a report that
    /// hid its own blind spots would read as a complete account.
    pub unreadable: Vec<String>,
}

/// The report between `before` and `after`.
#[must_use]
pub fn job_report(before: &Manifest, after: &Manifest) -> JobReport {
    JobReport {
        finished_at_ms: now_ms(),
        unreadable: merge_unread(&before.unread, &after.unread),
        changes: ChangeSet::between(before, after),
    }
}

/// Now, in milliseconds since the Unix epoch; `0` on a clock set before it.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: fixtures writing, renaming and deleting files to stand in \
              for a job, so there is something to report; the production code in this file \
              touches no filesystem at all"
)]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;
    use crate::workspace::changes::Change;

    #[test]
    fn the_report_names_each_change() {
        let dir = workshop("report-report");
        let folder = dir.path();
        std::fs::write(folder.join("invoices-2026.xlsx"), b"v1").expect("fixture");
        let before = Manifest::of_folder(folder).expect("baseline");

        std::fs::write(folder.join("invoices-2026.xlsx"), b"v2 is longer").expect("job");
        std::fs::create_dir(folder.join("reminder-letters")).expect("job");
        let after = Manifest::of_folder(folder).expect("closing walk");

        let report = job_report(&before, &after);
        assert!(report.finished_at_ms > 0);
        assert!(report.changes.changes.contains(&Change::Modified {
            path: "invoices-2026.xlsx".into(),
        }));
        assert!(report.changes.changes.contains(&Change::Created {
            path: "reminder-letters".into(),
            folder: true,
        }));
    }

    #[test]
    fn a_quiet_job_says_so() {
        let dir = workshop("report-quiet");
        let folder = dir.path();
        std::fs::write(folder.join("a.txt"), b"same").expect("fixture");
        let before = Manifest::of_folder(folder).expect("baseline");
        let after = Manifest::of_folder(folder).expect("closing walk");

        assert!(job_report(&before, &after).changes.changes.is_empty());
    }

    /// A rename over the real filesystem, end to end: the move preserves
    /// size and timestamp, which is exactly what pairs the two names.
    #[test]
    fn a_renamed_file_is_reported_as_one_change_with_two_names() {
        let dir = workshop("report-rename");
        let folder = dir.path();
        std::fs::write(folder.join("scan001.pdf"), b"the scan").expect("fixture");
        let before = Manifest::of_folder(folder).expect("baseline");

        std::fs::rename(folder.join("scan001.pdf"), folder.join("enrolment.pdf")).expect("job");
        let after = Manifest::of_folder(folder).expect("closing walk");

        assert_eq!(
            job_report(&before, &after).changes.changes,
            vec![Change::Renamed {
                from: "scan001.pdf".into(),
                to: "enrolment.pdf".into(),
            }]
        );
    }

    /// The report says what one walk could not see, rather than presenting
    /// a partial account as a complete one.
    #[test]
    fn what_neither_walk_could_read_is_named_in_the_report() {
        let dir = workshop("report-unreadable");
        let folder = dir.path();
        let mut before = Manifest::of_folder(folder).expect("baseline");
        before.unread = vec!["scans".to_string()];
        let mut after = Manifest::of_folder(folder).expect("closing walk");
        after.unread = vec!["scans/2026".to_string()];

        let report = job_report(&before, &after);
        assert_eq!(report.unreadable, vec!["scans".to_string()]);
    }
}
