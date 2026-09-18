//! What a job changed: the difference between two manifests.

use crate::workspace::manifest::{ContentHash, EntryKind, Manifest};
use crate::workspace::restore::covers;
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
    Modified {
        path: String,
    },
    Deleted {
        path: String,
        folder: bool,
    },
    /// A deleted and a created file with identical bytes, paired.
    Renamed {
        from: String,
        to: String,
    },
}

impl Change {
    /// The path this change is filed under, for ordering.
    fn key(&self) -> &str {
        match self {
            Self::Created { path, .. } | Self::Modified { path } | Self::Deleted { path, .. } => {
                path
            }
            Self::Renamed { from, .. } => from,
        }
    }
}

/// Every change between two states of the folder, ordered by path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
}

impl ChangeSet {
    /// Diff `after` against `before`.  A deleted and a created file with
    /// the same content hash pair up as a rename, in path order — unless
    /// the content is empty (an empty file carries no evidence of
    /// identity: every placeholder and `.gitkeep` hashes the same) or the
    /// hash has more than one candidate on both the deleted and the
    /// created side (mass duplication is not a mass rename, and guessing
    /// a pairing among several equally-plausible ones would be a lie); in
    /// either case the files are reported honestly as an unpaired
    /// deletion and creation instead.
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
                Some(_) => {}
            }
        }
        for (path, kind) in &after.entries {
            if !before.entries.contains_key(path) && !unreadable(path) {
                created.push((path, kind));
            }
        }

        let mut unclaimed: BTreeMap<&ContentHash, VecDeque<usize>> = BTreeMap::new();
        for (i, (_, kind)) in created.iter().enumerate() {
            if let EntryKind::File { hash, size, .. } = kind
                && *size != 0
            {
                unclaimed.entry(hash).or_default().push_back(i);
            }
        }
        // How many deleted files share each hash, counted up front (the
        // loop below consumes `deleted`).  A hash with more than one
        // candidate on both sides is mass duplication, not a rename: there
        // is no honest way to pick which deletion matches which creation.
        let mut deleted_hash_counts: BTreeMap<&ContentHash, usize> = BTreeMap::new();
        for (_, kind) in &deleted {
            if let EntryKind::File { hash, size, .. } = kind
                && *size != 0
            {
                *deleted_hash_counts.entry(hash).or_default() += 1;
            }
        }

        let mut renamed_to = vec![false; created.len()];
        for (path, kind) in deleted {
            if let EntryKind::File { hash, size, .. } = kind
                && *size != 0
                && !(deleted_hash_counts.get(hash).is_some_and(|&n| n > 1)
                    && unclaimed.get(hash).is_some_and(|q| q.len() > 1))
                && let Some(i) = unclaimed.get_mut(hash).and_then(VecDeque::pop_front)
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
    use crate::workspace::manifest::ContentHash;

    fn file(bytes: &[u8]) -> EntryKind {
        EntryKind::File {
            size: bytes.len() as u64,
            hash: ContentHash::of_bytes(bytes),
            mode: 0o644,
            mtime_ns: 0,
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
            ("kept.txt", file(b"same")),
            ("edited.txt", file(b"old")),
            ("gone.txt", file(b"unique bytes")),
        ]);
        let after = manifest(&[
            ("kept.txt", file(b"same")),
            ("edited.txt", file(b"new")),
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

    /// A file whose bytes are untouched but whose timestamp moved — a
    /// backup agent, a sync client, a tool that rewrote it identically —
    /// did not change, and must not be reported as though the assistant
    /// had changed it.
    #[test]
    fn a_touched_but_unedited_file_is_not_a_change() {
        let before = manifest(&[("notes.txt", file(b"same"))]);
        let after = manifest(&[(
            "notes.txt",
            EntryKind::File {
                size: 4,
                hash: ContentHash::of_bytes(b"same"),
                mode: 0o644,
                mtime_ns: 1_700_000_000_000_000_000,
            },
        )]);

        let set = ChangeSet::between(&before, &after);
        assert!(
            set.changes.is_empty(),
            "a timestamp is not an edit: {:?}",
            set.changes
        );
    }

    #[test]
    fn identical_bytes_moved_elsewhere_read_as_a_rename() {
        let before = manifest(&[("drafts/offer.docx", file(b"the offer"))]);
        let after = manifest(&[("sent/offer.docx", file(b"the offer"))]);

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
            vec![Change::Renamed {
                from: "drafts/offer.docx".into(),
                to: "sent/offer.docx".into(),
            }]
        );
    }

    /// The invariant the whole change exists for: a `before` that could
    /// not read a subtree must not turn that subtree's files, present in
    /// `after`, into reported creations.
    #[test]
    fn a_subtree_unread_in_before_is_not_reported_created_in_after() {
        let mut before = manifest(&[("kept.txt", file(b"same"))]);
        before.unread = vec!["scans".to_string()];
        let after = manifest(&[
            ("kept.txt", file(b"same")),
            ("scans", EntryKind::Folder),
            ("scans/photo.jpg", file(b"a photo")),
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
            ("kept.txt", file(b"same")),
            ("scans", EntryKind::Folder),
            ("scans/photo.jpg", file(b"a photo")),
        ]);
        let mut after = manifest(&[("kept.txt", file(b"same"))]);
        after.unread = vec!["scans".to_string()];

        let set = ChangeSet::between(&before, &after);
        assert!(
            set.changes.is_empty(),
            "an unread subtree must not manufacture deletions: {:?}",
            set.changes
        );
    }

    /// An empty file carries no evidence of identity — every placeholder,
    /// lock file, or `.gitkeep` hashes the same as every other.  Deleting
    /// one empty file and creating an unrelated empty file elsewhere must
    /// not be reported as a rename, or the change report lies about which
    /// two paths are connected.
    #[test]
    fn empty_files_never_pair_into_a_rename() {
        let before = manifest(&[("old/placeholder", file(b""))]);
        let after = manifest(&[("new/gitkeep", file(b""))]);

        let set = ChangeSet::between(&before, &after);
        assert_eq!(
            set.changes,
            vec![
                Change::Created {
                    path: "new/gitkeep".into(),
                    folder: false,
                },
                Change::Deleted {
                    path: "old/placeholder".into(),
                    folder: false,
                },
            ]
        );
    }

    /// A hash shared by more than one deleted file and more than one
    /// created file is mass duplication (a boilerplate file copied around
    /// a folder), not a mass rename: there is no honest way to say which
    /// deletion matches which creation, so all of them are reported as
    /// plain deletions and creations.
    #[test]
    fn mass_duplicated_content_is_not_paired_into_renames() {
        let before = manifest(&[
            ("a/boilerplate.txt", file(b"template")),
            ("b/boilerplate.txt", file(b"template")),
        ]);
        let after = manifest(&[
            ("c/boilerplate.txt", file(b"template")),
            ("d/boilerplate.txt", file(b"template")),
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
            ("edited.txt", file(b"old")),
            ("scans/photo.jpg", file(b"a photo")),
        ]);
        before.unread = vec!["scans".to_string()];
        let after = manifest(&[
            ("edited.txt", file(b"new")),
            ("scans/photo.jpg", file(b"a photo")),
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
