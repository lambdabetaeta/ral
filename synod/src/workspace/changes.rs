//! What a job changed: the difference between two manifests.
//!
//! Both manifests are stat-only, so this is an account of what the
//! filesystem said, never of what any file holds.  Where that is enough to
//! prove a change it says so ([`Change::Modified`]); where it is not, it
//! says *that* instead ([`Change::Touched`]) rather than guessing.

use crate::workspace::manifest::{EntryKind, Manifest, covers};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::VecDeque;

/// One change to one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    Created {
        path: String,
        folder: bool,
    },
    /// The size — or, on a Unix host, the permission bits — differ.  The
    /// entry is definitely not what it was.
    Modified {
        path: String,
    },
    /// The timestamp moved but nothing else did.  Something wrote to this
    /// file; whether the bytes differ cannot be known without reading them,
    /// and synod no longer reads them.  Reported as its own thing precisely
    /// so it is never presented as an edit: a backup agent, a sync client,
    /// or a tool that rewrote a file with the bytes it already had lands
    /// here too.
    Touched {
        path: String,
    },
    Deleted {
        path: String,
        folder: bool,
    },
    /// A deleted and a created file whose size and timestamp both match,
    /// paired.  A move preserves both, so the pair is strong evidence —
    /// stronger than identical content would be, since content is shared
    /// by every copy of a file and a timestamp to the nanosecond is not.
    Renamed {
        from: String,
        to: String,
    },
}

impl Change {
    /// The path this change is filed under, for ordering.
    fn key(&self) -> &str {
        match self {
            Self::Created { path, .. }
            | Self::Modified { path }
            | Self::Touched { path }
            | Self::Deleted { path, .. } => path,
            Self::Renamed { from, .. } => from,
        }
    }
}

/// Every change between two states of the folder, ordered by path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
}

/// What a rename is matched on: a file's size and the nanosecond its
/// contents were last written.  A rename or a move preserves both.
type Fingerprint = (u64, u64);

/// The rename fingerprint of an entry, or `None` for anything that cannot
/// be renamed into or out of — a folder or a link, which are recreated
/// rather than moved as far as a stat-walk can tell.
fn fingerprint(kind: &EntryKind) -> Option<Fingerprint> {
    match kind {
        EntryKind::File { size, mtime_ns, .. } => Some((*size, *mtime_ns)),
        EntryKind::Folder | EntryKind::Link { .. } => None,
    }
}

impl ChangeSet {
    /// Diff `after` against `before`.
    ///
    /// A deleted and a created file pair up as a rename when their size and
    /// timestamp both match, in path order — unless that fingerprint has
    /// more than one candidate on both the deleted and the created side
    /// (mass duplication is not a mass rename, and guessing a pairing among
    /// several equally-plausible ones would be a lie), in which case the
    /// files are reported honestly as an unpaired deletion and creation
    /// instead.
    ///
    /// A path covered by either manifest's `unread` is left out of the
    /// diff entirely: a partial `before` cannot manufacture a creation, and
    /// a partial `after` cannot manufacture a deletion.
    pub fn between(before: &Manifest, after: &Manifest) -> Self {
        let unreadable = |path: &str| {
            before
                .unread
                .iter()
                .chain(&after.unread)
                .any(|root| covers(path, root))
        };

        let mut changes = Vec::new();
        let mut deleted: Vec<(&str, &EntryKind)> = Vec::new();
        let mut created: Vec<(&str, &EntryKind)> = Vec::new();

        for (path, kind) in &before.entries {
            if unreadable(path) {
                continue;
            }
            match after.entries.get(path) {
                None => deleted.push((path, kind)),
                Some(now) if !now.same_as(kind) => {
                    changes.push(Change::Modified { path: path.clone() });
                }
                // The size and mode stand; only the clock moved.  Something
                // wrote here, and no stat-walk can say whether it wrote
                // anything different.
                Some(now) if fingerprint(now) != fingerprint(kind) => {
                    changes.push(Change::Touched { path: path.clone() });
                }
                Some(_) => {}
            }
        }
        for (path, kind) in &after.entries {
            if !before.entries.contains_key(path) && !unreadable(path) {
                created.push((path, kind));
            }
        }

        let mut unclaimed: BTreeMap<Fingerprint, VecDeque<usize>> = BTreeMap::new();
        for (i, (_, kind)) in created.iter().enumerate() {
            if let Some(print) = fingerprint(kind) {
                unclaimed.entry(print).or_default().push_back(i);
            }
        }
        // How many deleted files share each fingerprint, counted up front
        // (the loop below consumes `deleted`).  A fingerprint with more than
        // one candidate on both sides is mass duplication, not a rename:
        // there is no honest way to pick which deletion matches which
        // creation.
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
                    folder: matches!(kind, EntryKind::Folder),
                });
            }
        }

        changes.sort_by(|a, b| a.key().cmp(b.key()));
        Self { changes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file of `size` bytes, last written at `mtime_ns`.
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

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
            vec![
                Change::Modified {
                    path: "edited.txt".into(),
                },
                Change::Created {
                    path: "fresh".into(),
                    folder: true,
                },
                Change::Deleted {
                    path: "gone.txt".into(),
                    folder: false,
                },
            ]
        );
    }

    /// The distinction the whole stat-only model turns on: a file whose
    /// timestamp moved while its size did not was written to, but nothing
    /// short of reading it can say whether its contents differ.  Reporting
    /// that as an edit would be a claim synod cannot stand behind.
    #[test]
    fn a_file_whose_timestamp_alone_moved_is_touched_not_modified() {
        let before = manifest(&[("notes.txt", file(4, 100))]);
        let after = manifest(&[("notes.txt", file(4, 1_700_000_000_000_000_000))]);

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
            vec![Change::Touched {
                path: "notes.txt".into(),
            }]
        );
    }

    /// The mirror: a file nothing touched at all is no change whatever, so
    /// a quiet job still reads as quiet.
    #[test]
    fn an_untouched_file_is_no_change_at_all() {
        let before = manifest(&[("notes.txt", file(4, 100))]);
        let after = manifest(&[("notes.txt", file(4, 100))]);
        assert!(ChangeSet::between(&before, &after).changes.is_empty());
    }

    /// A move preserves size and timestamp both, which is what pairs the
    /// deletion with the creation.
    #[test]
    fn a_file_moved_elsewhere_reads_as_a_rename() {
        let before = manifest(&[("drafts/offer.docx", file(9, 1_234))]);
        let after = manifest(&[("sent/offer.docx", file(9, 1_234))]);

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
            vec![Change::Renamed {
                from: "drafts/offer.docx".into(),
                to: "sent/offer.docx".into(),
            }]
        );
    }

    /// Same size, different timestamp: a file written fresh at the new
    /// name, not the old one moved.  Pairing these would connect two paths
    /// that have nothing to do with each other.
    #[test]
    fn a_same_sized_file_written_fresh_is_not_a_rename() {
        let before = manifest(&[("old/a.txt", file(9, 1_234))]);
        let after = manifest(&[("new/b.txt", file(9, 9_999))]);

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
            vec![
                Change::Created {
                    path: "new/b.txt".into(),
                    folder: false,
                },
                Change::Deleted {
                    path: "old/a.txt".into(),
                    folder: false,
                },
            ]
        );
    }

    /// The invariant the unread list exists for: a `before` that could
    /// not read a subtree must not turn that subtree's files, present in
    /// `after`, into reported creations.
    #[test]
    fn a_subtree_unread_in_before_is_not_reported_created_in_after() {
        let mut before = manifest(&[("kept.txt", file(4, 100))]);
        before.unread = vec!["scans".to_string()];
        let after = manifest(&[
            ("kept.txt", file(4, 100)),
            ("scans", EntryKind::Folder),
            ("scans/photo.jpg", file(7, 100)),
        ]);

        let set = ChangeSet::between(&before, &after);
        assert!(
            set.changes.is_empty(),
            "an unread subtree must not manufacture creations: {:?}",
            set.changes
        );
    }

    /// The mirror case: a subtree unread in `after` must not turn its
    /// files, present in `before`, into reported deletions.
    #[test]
    fn a_subtree_unread_in_after_is_not_reported_deleted_in_before() {
        let before = manifest(&[
            ("kept.txt", file(4, 100)),
            ("scans", EntryKind::Folder),
            ("scans/photo.jpg", file(7, 100)),
        ]);
        let mut after = manifest(&[("kept.txt", file(4, 100))]);
        after.unread = vec!["scans".to_string()];

        let set = ChangeSet::between(&before, &after);
        assert!(
            set.changes.is_empty(),
            "an unread subtree must not manufacture deletions: {:?}",
            set.changes
        );
    }

    /// A fingerprint shared by more than one deleted file and more than one
    /// created file is mass duplication (a boilerplate file copied around a
    /// folder in one go, so every copy carries the same size and the same
    /// instant), not a mass rename: there is no honest way to say which
    /// deletion matches which creation, so all of them are reported as
    /// plain deletions and creations.
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

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
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
                    folder: false,
                },
                Change::Created {
                    path: "d/boilerplate.txt".into(),
                    folder: false,
                },
            ]
        );
    }

    /// Only the unread subtree is suppressed; an ordinary change elsewhere
    /// in the same diff still reports.
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

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
            vec![Change::Modified {
                path: "edited.txt".into(),
            }]
        );
    }
}
