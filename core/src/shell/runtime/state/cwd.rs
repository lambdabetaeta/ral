//! Logical working directory and path-resolution verbs.
//!
//! `cd` moves the shell-owned [`CwdPath`] on `context.cwd`, never the process cwd:
//! that one is OS-global, and a worker (`spawn`, `watch`) or a pipeline
//! stage would see a sibling's `cd` as a sudden reorder.  Children still land right —
//! `apply_env` in `core/src/shell/exec/command/process.rs` passes [`Shell::cwd`]
//! as `Command::current_dir` and exports it as `PWD`.

use super::Shell;
use crate::path::{CwdPath, walk_dir};
use crate::shell::runtime::Error;
use std::io::ErrorKind;
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
        let dir = walk_dir(&resolved).map_err(|e| match e.kind() {
            ErrorKind::NotADirectory => {
                Error::new(format!("{}: not a directory", resolved.shown()))
            }
            _ => Error::io(resolved.shown(), &e),
        })?;
        let cwd = CwdPath::new(dir.real().as_path().to_path_buf())
            .map_err(|e| Error::new(e.to_string()))?;
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
