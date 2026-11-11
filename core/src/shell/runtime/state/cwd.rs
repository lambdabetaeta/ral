//! Logical working directory and path-resolution verbs.
//!
//! `cd` moves the shell-owned [`CwdPath`] on `context.cwd`, never the process cwd:
//! that one is OS-global, and a worker (`spawn`, `watch`) or a pipeline
//! stage would see a sibling's `cd` as a sudden reorder.  Children still land right —
//! `apply_env` in `core/src/shell/exec/command.rs` passes [`Shell::cwd`]
//! as `Command::current_dir` and exports it as `PWD`.

use super::Shell;
use crate::fact::{InvalidCause, IoOperation, IoSubject};
use crate::path::{CwdPath, walk_dir};
use crate::shell::runtime::Error;
use crate::shell::runtime::invalid;
use crate::text::quote_native;
use std::path::PathBuf;

impl Shell {
    /// The logical cwd, [`Context::launch_cwd`](super::Context::launch_cwd).
    ///
    /// Every path-resolving builtin routes through here, so a `within` scope or
    /// a prior `cd` binds the whole interpreter, not just spawned children.
    pub fn cwd(&self) -> PathBuf {
        self.context.launch_cwd()
    }

    /// State the logical cwd outright, overriding the one the shell was
    /// constructed at, for a host that never `chdir`s the process.
    pub fn seed_cwd(&mut self, cwd: CwdPath) {
        self.context.cwd = cwd;
    }

    /// `within [dir: …]`'s entry: the cell becomes `dir`, and the prior cell
    /// comes back for [`Self::restore_cwd`].  Not a `cd`, so no `chdir`
    /// authority is asked.
    pub(crate) fn enter_cwd(&mut self, dir: CwdPath) -> CwdPath {
        std::mem::replace(&mut self.context.cwd, dir)
    }

    /// Put back a cell [`Self::enter_cwd`] saved, undoing every `cd` since.
    pub(crate) fn restore_cwd(&mut self, saved: CwdPath) {
        self.context.cwd = saved;
    }

    /// Move the cwd to the directory `target` names, resolved as every path
    /// builtin resolves (see [`Self::resolve`]): the cwd is where the walk of
    /// it lands.
    ///
    /// # Errors
    /// If the resolved target cannot be walked, is not a directory, or is no
    /// working directory ([`CwdPath`]).
    pub(crate) fn apply_chdir(&mut self, target: &str) -> Result<(), Error> {
        let resolved = self.resolve(target).folded();
        let dir = walk_dir(&resolved).map_err(|e| {
            Error::io(
                IoOperation::Chdir,
                IoSubject::Path(quote_native(resolved.as_path().as_os_str())),
                &e,
            )
        })?;
        let cwd = CwdPath::new(dir.real().as_path().to_path_buf())
            .map_err(|e| invalid("cd", InvalidCause::Path((&e).into())))?;
        self.context.cwd = cwd;
        Ok(())
    }

    /// Resolve `path` against the effective cwd, minting a
    /// [`crate::path::LexicalPath`] that the fs guards consume directly; a
    /// caller that opens the file takes `.into_inner()` / `.as_path()`.
    pub fn resolve(&self, path: &str) -> crate::path::LexicalPath {
        self.context.resolver().resolve(path)
    }

    /// [`Self::resolve`] for a path the host already holds, taken as it is:
    /// see [`crate::path::Resolver::anchor`].
    pub fn anchor(&self, path: &std::path::Path) -> crate::path::LexicalPath {
        self.context.resolver().anchor(path)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::fact::{IoKind, IoLeaf, Reason};

    #[test]
    fn apply_chdir_keeps_the_requested_path_and_does_not_move_on_failure() {
        let mut shell = crate::test_helper::core_shell();
        let before = shell.cwd();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        for path in [file, tmp.path().join("absent")] {
            let target = path.to_str().unwrap();
            let error = shell.apply_chdir(target).unwrap_err();
            let Reason::Io(failure) = &error.reason else {
                panic!("I/O refusal")
            };
            assert_eq!(failure.operation, IoOperation::Chdir);
            assert_eq!(
                failure.subject,
                IoSubject::Path(quote_native(
                    shell.resolve(target).folded().as_path().as_os_str()
                ))
            );
            assert!(
                matches!(failure.cause.leaf.as_ref(), IoLeaf::Os(os) if matches!(os.kind, IoKind::NotADirectory | IoKind::NotFound))
            );
            assert_eq!(shell.cwd(), before);
        }
    }
}
