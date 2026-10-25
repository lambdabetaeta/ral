//! The live shell's boundary over the directory-tree walk.

use super::{Break, Settled, Shell, sig};
use crate::path::CwdPath;
use crate::path::tree::walk::{Entries, Found, Walk, WalkEntry};

/// A walk judged by the shell's live grant, named as its caller asked.
pub struct AdmittedWalk {
    entries: Entries,
    base: Option<CwdPath>,
    who: &'static str,
}

impl Shell {
    /// Judge `walk` by the live grant.  A cwd-relative `named` gives
    /// cwd-relative entry names where possible; other roots give absolute
    /// ones.  `who` heads a name that no ral string can hold.
    pub fn admitted_walk(&self, walk: Walk, who: &'static str, named: &str) -> AdmittedWalk {
        // If the cwd cannot be resolved, names stay absolute.
        let base = (self.context.resolver().is_relative(named))
            .then(|| self.resolve(".").into_cwd().ok())
            .flatten();
        let region = self.context.grants.read_region(self.baseline());
        AdmittedWalk {
            entries: walk.readable_by(region).entries(self.scope.clone()),
            base,
            who,
        }
    }
}

impl AdmittedWalk {
    /// The next entry seen; entries the walk could not see are skipped.
    ///
    /// # Errors
    /// The scope's cancellation, as a non-catchable stop.
    #[allow(
        clippy::should_implement_trait,
        reason = "a stop is an error that ends the walk, not an item"
    )]
    pub fn next(&mut self) -> Settled<Option<WalkEntry>> {
        for entry in self.entries.by_ref() {
            let entry = entry.map_err(Break::from)?;
            if !matches!(entry.found, Found::Unseen(_)) {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// `entry`'s name in the shape the root was named.
    ///
    /// # Errors
    /// A name no ral string can hold, headed by what the walk was for.
    pub fn name(&self, entry: &WalkEntry) -> Settled<String> {
        entry
            .name(self.base.as_ref())
            .map_err(|e| sig(format!("{}: {e}", self.who)))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::path::{LexicalPath, utf8};
    use crate::process::CancelCause;
    use crate::shell::runtime::Escape;

    #[test]
    fn an_admitted_walk_that_is_cancelled_aborts() {
        let shell = crate::test_helper::core_shell();
        let owner = shell.cancel_handle();
        owner.cancel(CancelCause::Interrupted);
        let root = shell.anchor(&shell.cwd());
        let mut walk = shell.admitted_walk(Walk::respecting_ignores(&root), "walk", ".");
        let stop = walk.next().expect_err("a cancelled scope aborts the walk");
        assert!(matches!(
            stop,
            Break::Escape(Escape::Stopped {
                cause: CancelCause::Interrupted,
                owner: Some(id),
                at: None,
            }) if id == owner.id()
        ));
        assert!(walk.next().unwrap().is_none());
    }

    #[test]
    fn cancellation_between_entries_keeps_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        let shell = crate::test_helper::core_shell();
        let root = LexicalPath::absolute(dir.path()).unwrap();
        let mut walk = shell.admitted_walk(Walk::all(&root), "walk", utf8(dir.path()).unwrap());
        assert_eq!(walk.next().unwrap().unwrap().depth, 0);

        let owner = shell.cancel_handle();
        owner.cancel(CancelCause::TimedOut);
        assert!(matches!(
            walk.next(),
            Err(Break::Escape(Escape::Stopped {
                cause: CancelCause::TimedOut,
                owner: Some(id),
                at: None,
            })) if id == owner.id()
        ));
        assert!(walk.next().unwrap().is_none());
    }

    #[test]
    fn an_unseen_root_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let shell = crate::test_helper::core_shell();
        let root = LexicalPath::absolute(&dir.path().join("missing")).unwrap();
        let mut walk = shell.admitted_walk(Walk::all(&root), "walk", utf8(root.as_path()).unwrap());
        assert!(walk.next().unwrap().is_none());
    }

    #[test]
    fn relative_and_absolute_roots_keep_their_name_shape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        let shell = Shell::root(
            crate::test_helper::core_runtime().into(),
            CwdPath::new(dir.path().to_owned()).unwrap(),
        );
        let root = shell.resolve("file");
        let absolute = crate::path::shown(root.as_path());
        for (named, expected) in [("file", "file"), (absolute.as_str(), absolute.as_str())] {
            let mut walk = shell.admitted_walk(Walk::all(&root), "walk", named);
            let entry = walk.next().unwrap().unwrap();
            assert_eq!(walk.name(&entry).unwrap(), expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unnameable_entry_raises_a_contextual_error() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(OsStr::from_bytes(b"bad-\xff")), "").unwrap();
        let shell = crate::test_helper::core_shell();
        let root = LexicalPath::absolute(dir.path()).unwrap();
        let mut walk = shell.admitted_walk(Walk::all(&root), "glob", utf8(dir.path()).unwrap());
        assert_eq!(walk.next().unwrap().unwrap().depth, 0);
        let entry = walk.next().unwrap().unwrap();
        let Break::Error(error) = walk.name(&entry).expect_err("no string can hold the name")
        else {
            panic!("an unnameable entry is an error, not a stop");
        };
        assert!(error.message.starts_with("glob: "), "{error:?}");
        assert!(error.message.contains("bad-\\xFF"), "{error:?}");
        assert!(error.message.contains("not valid UTF-8"), "{error:?}");
        assert!(error.message.ends_with("rename it?"), "{error:?}");
    }
}
