//! Stat-only changes distinguish evidence of modification from a timestamp-only touch.

use crate::folder::manifest::{EntryKind, Manifest, covers, merge_unread};
use std::collections::BTreeMap;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    Created {
        path: String,
        now: EntryKind,
    },
    Modified {
        path: String,
        now: EntryKind,
    },
    /// Only the timestamp differs; file contents are unknown.
    Touched {
        path: String,
    },
    Deleted {
        path: String,
        folder: bool,
    },
    /// Files paired by equal size and nanosecond mtime.
    Renamed {
        from: String,
        to: String,
    },
}

impl Change {
    fn key(&self) -> &str {
        match self {
            Self::Created { path, .. }
            | Self::Modified { path, .. }
            | Self::Touched { path }
            | Self::Deleted { path, .. } => path,
            Self::Renamed { from, .. } => from,
        }
    }

    pub(crate) fn leaves_file(&self) -> bool {
        match self {
            Self::Created { now, .. } | Self::Modified { now, .. } => {
                matches!(now, EntryKind::File { .. })
            }
            Self::Touched { .. } | Self::Renamed { .. } => true,
            Self::Deleted { .. } => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobReport {
    pub(crate) changes: Vec<Change>,
    pub(crate) unreadable: Vec<String>,
}

type Fingerprint = (u64, u64);

fn fingerprint(kind: &EntryKind) -> Option<Fingerprint> {
    match kind {
        EntryKind::File { size, mtime_ns, .. } => Some((*size, *mtime_ns)),
        EntryKind::Folder | EntryKind::Link { .. } => None,
    }
}

/// Suppress unread paths; pair fingerprints unless both sides have multiple candidates.
#[must_use]
pub(crate) fn job_report(before: &Manifest, after: &Manifest) -> JobReport {
    let unreadable = merge_unread(&before.unread, &after.unread);

    let mut changes = Vec::new();
    let mut deleted: Vec<(&str, &EntryKind)> = Vec::new();
    let mut created: Vec<(&str, &EntryKind)> = Vec::new();

    for (path, kind) in &before.entries {
        if unreadable.iter().any(|root| covers(path, root)) {
            continue;
        }
        match after.entries.get(path) {
            None => deleted.push((path, kind)),
            Some(now) if !now.same_as(kind) => {
                changes.push(Change::Modified {
                    path: path.clone(),
                    now: now.clone(),
                });
            }
            Some(now) if fingerprint(now) != fingerprint(kind) => {
                changes.push(Change::Touched { path: path.clone() });
            }
            Some(_) => {}
        }
    }
    for (path, kind) in &after.entries {
        if !before.entries.contains_key(path) && !unreadable.iter().any(|root| covers(path, root)) {
            created.push((path, kind));
        }
    }

    let mut unclaimed: BTreeMap<Fingerprint, VecDeque<usize>> = BTreeMap::new();
    for (i, (_, kind)) in created.iter().enumerate() {
        if let Some(print) = fingerprint(kind) {
            unclaimed.entry(print).or_default().push_back(i);
        }
    }
    // Count before consuming deleted: ambiguity on both sides forbids pairing.
    let mut deleted_counts: BTreeMap<Fingerprint, usize> = BTreeMap::new();
    for (_, kind) in &deleted {
        if let Some(print) = fingerprint(kind) {
            *deleted_counts.entry(print).or_default() += 1;
        }
    }

    let mut renamed_to = vec![false; created.len()];
    for (path, kind) in deleted {
        if let Some(print) = fingerprint(kind)
            && !(deleted_counts.get(&print).is_some_and(|&n| n > 1)
                && unclaimed.get(&print).is_some_and(|q| q.len() > 1))
            && let Some(i) = unclaimed.get_mut(&print).and_then(VecDeque::pop_front)
        {
            renamed_to[i] = true;
            changes.push(Change::Renamed {
                from: path.to_string(),
                to: created[i].0.to_string(),
            });
            continue;
        }
        changes.push(Change::Deleted {
            path: path.to_string(),
            folder: matches!(kind, EntryKind::Folder),
        });
    }
    for (i, (path, kind)) in created.iter().enumerate() {
        if !renamed_to[i] {
            changes.push(Change::Created {
                path: (*path).to_string(),
                now: (*kind).clone(),
            });
        }
    }

    changes.sort_by(|a, b| a.key().cmp(b.key()));
    JobReport {
        changes,
        unreadable,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;

    fn file(size: u64, mtime_ns: u64) -> EntryKind {
        EntryKind::File {
            size,
            mtime_ns,
            mode: 0o644,
        }
    }

    fn manifest(entries: &[(&str, EntryKind)]) -> Manifest {
        Manifest {
            entries: entries
                .iter()
                .map(|(path, kind)| ((*path).to_string(), kind.clone()))
                .collect(),
            unread: Vec::new(),
        }
    }

    #[test]
    fn created_modified_and_deleted_are_told_apart() {
        let before = manifest(&[
            ("kept.txt", file(4, 100)),
            ("edited.txt", file(3, 100)),
            ("gone.txt", file(12, 100)),
        ]);
        let after = manifest(&[
            ("kept.txt", file(4, 100)),
            ("edited.txt", file(9, 200)),
            ("fresh", EntryKind::Folder),
        ]);

        let report = job_report(&before, &after);
        assert_eq!(
            report.changes,
            vec![
                Change::Modified {
                    path: "edited.txt".into(),
                    now: file(9, 200),
                },
                Change::Created {
                    path: "fresh".into(),
                    now: EntryKind::Folder,
                },
                Change::Deleted {
                    path: "gone.txt".into(),
                    folder: false,
                },
            ]
        );
    }

    #[test]
    fn a_change_says_whether_a_file_now_stands_at_its_path() {
        let link = EntryKind::Link {
            target: "elsewhere".into(),
        };
        let before = manifest(&[("was-a-file", file(3, 100))]);
        let after = manifest(&[
            ("was-a-file", EntryKind::Folder),
            ("shortcut", link.clone()),
        ]);

        let report = job_report(&before, &after);
        assert_eq!(
            report.changes,
            vec![
                Change::Created {
                    path: "shortcut".into(),
                    now: link,
                },
                Change::Modified {
                    path: "was-a-file".into(),
                    now: EntryKind::Folder,
                },
            ]
        );
        assert!(report.changes.iter().all(|change| !change.leaves_file()));
    }

    #[test]
    fn a_file_whose_timestamp_alone_moved_is_touched_not_modified() {
        let before = manifest(&[("notes.txt", file(4, 100))]);
        let after = manifest(&[("notes.txt", file(4, 1_700_000_000_000_000_000))]);

        let report = job_report(&before, &after);
        assert_eq!(
            report.changes,
            vec![Change::Touched {
                path: "notes.txt".into(),
            }]
        );
    }

    #[test]
    fn an_untouched_file_is_no_change_at_all() {
        let before = manifest(&[("notes.txt", file(4, 100))]);
        let after = manifest(&[("notes.txt", file(4, 100))]);
        assert_eq!(job_report(&before, &after).changes, Vec::<Change>::new());
    }

    #[test]
    fn a_file_moved_elsewhere_reads_as_a_rename() {
        let before = manifest(&[("drafts/offer.docx", file(9, 1_234))]);
        let after = manifest(&[("sent/offer.docx", file(9, 1_234))]);

        let report = job_report(&before, &after);
        assert_eq!(
            report.changes,
            vec![Change::Renamed {
                from: "drafts/offer.docx".into(),
                to: "sent/offer.docx".into(),
            }]
        );
    }

    #[test]
    fn a_same_sized_file_written_fresh_is_not_a_rename() {
        let before = manifest(&[("old/a.txt", file(9, 1_234))]);
        let after = manifest(&[("new/b.txt", file(9, 9_999))]);

        let report = job_report(&before, &after);
        assert_eq!(
            report.changes,
            vec![
                Change::Created {
                    path: "new/b.txt".into(),
                    now: file(9, 9_999),
                },
                Change::Deleted {
                    path: "old/a.txt".into(),
                    folder: false,
                },
            ]
        );
    }

    #[test]
    fn a_subtree_unread_in_before_is_not_reported_created_in_after() {
        let mut before = manifest(&[("kept.txt", file(4, 100))]);
        before.unread = vec!["scans".to_string()];
        let after = manifest(&[
            ("kept.txt", file(4, 100)),
            ("scans", EntryKind::Folder),
            ("scans/photo.jpg", file(7, 100)),
        ]);

        let report = job_report(&before, &after);
        assert!(
            report.changes.is_empty(),
            "an unread subtree must not manufacture creations: {:?}",
            report.changes
        );
    }

    #[test]
    fn a_subtree_unread_in_after_is_not_reported_deleted_in_before() {
        let before = manifest(&[
            ("kept.txt", file(4, 100)),
            ("scans", EntryKind::Folder),
            ("scans/photo.jpg", file(7, 100)),
        ]);
        let mut after = manifest(&[("kept.txt", file(4, 100))]);
        after.unread = vec!["scans".to_string()];

        let report = job_report(&before, &after);
        assert!(
            report.changes.is_empty(),
            "an unread subtree must not manufacture deletions: {:?}",
            report.changes
        );
    }

    #[test]
    fn mass_duplicated_files_are_not_paired_into_renames() {
        let before = manifest(&[
            ("a/boilerplate.txt", file(8, 500)),
            ("b/boilerplate.txt", file(8, 500)),
        ]);
        let after = manifest(&[
            ("c/boilerplate.txt", file(8, 500)),
            ("d/boilerplate.txt", file(8, 500)),
        ]);

        let report = job_report(&before, &after);
        assert_eq!(
            report.changes,
            vec![
                Change::Deleted {
                    path: "a/boilerplate.txt".into(),
                    folder: false,
                },
                Change::Deleted {
                    path: "b/boilerplate.txt".into(),
                    folder: false,
                },
                Change::Created {
                    path: "c/boilerplate.txt".into(),
                    now: file(8, 500),
                },
                Change::Created {
                    path: "d/boilerplate.txt".into(),
                    now: file(8, 500),
                },
            ]
        );
    }

    #[test]
    fn suppression_is_scoped_to_the_unread_key() {
        let mut before = manifest(&[
            ("edited.txt", file(3, 100)),
            ("scans/photo.jpg", file(7, 100)),
        ]);
        before.unread = vec!["scans".to_string()];
        let after = manifest(&[
            ("edited.txt", file(9, 200)),
            ("scans/photo.jpg", file(7, 100)),
        ]);

        let report = job_report(&before, &after);
        assert_eq!(
            report.changes,
            vec![Change::Modified {
                path: "edited.txt".into(),
                now: file(9, 200),
            }]
        );
    }

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
        assert!(report.changes.iter().any(|change| matches!(
            change,
            Change::Modified { path, now: EntryKind::File { size: 12, .. } }
                if path == "invoices-2026.xlsx"
        )));
        assert!(report.changes.contains(&Change::Created {
            path: "reminder-letters".into(),
            now: EntryKind::Folder,
        }));
    }

    #[test]
    fn a_quiet_job_says_so() {
        let dir = workshop("report-quiet");
        let folder = dir.path();
        std::fs::write(folder.join("a.txt"), b"same").expect("fixture");
        let before = Manifest::of_folder(folder).expect("baseline");
        let after = Manifest::of_folder(folder).expect("closing walk");

        assert_eq!(job_report(&before, &after).changes, Vec::<Change>::new());
    }

    #[test]
    fn a_renamed_file_is_reported_as_one_change_with_two_names() {
        let dir = workshop("report-rename");
        let folder = dir.path();
        std::fs::write(folder.join("scan001.pdf"), b"the scan").expect("fixture");
        let before = Manifest::of_folder(folder).expect("baseline");

        std::fs::rename(folder.join("scan001.pdf"), folder.join("enrolment.pdf")).expect("job");
        let after = Manifest::of_folder(folder).expect("closing walk");

        assert_eq!(
            job_report(&before, &after).changes,
            vec![Change::Renamed {
                from: "scan001.pdf".into(),
                to: "enrolment.pdf".into(),
            }]
        );
    }

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
