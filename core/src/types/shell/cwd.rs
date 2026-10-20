//! Logical working directory and path-resolution verbs.
//!
//! `cd` moves the shell-owned [`Cwd`] on `context.cwd`, never the process cwd:
//! that one is OS-global, and a worker (`spawn`, `watch`) or a pipeline
//! stage would see a sibling's `cd` as a sudden reorder.  Children still land right —
//! `apply_env` in `core/src/runtime/command/process.rs` passes [`Shell::cwd`]
//! as `Command::current_dir` and exports it as `PWD`.

use super::Shell;
use crate::path::CwdPath;
use crate::types::Error;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The working directory, one state cell: `cd` sets it, [`Shell::cwd`] gets
/// it, and `within [dir: …]` is its local-state handler, restoring the cell on
/// every exit.
///
/// It keeps no previous directory, hence no `OLDPWD`.  Never empty: a root
/// shell is constructed at one ([`Shell::root`]), and a child inherits its
/// parent's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cwd(pub(in crate::types::shell) CwdPath);

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
        self.context.cwd = Cwd(cwd);
    }

    /// `within [dir: …]`'s entry: the cell becomes `dir`, and the prior cell
    /// comes back for [`Self::restore_cwd`].  Not a `cd`, so no `chdir`
    /// authority is asked.
    pub(crate) fn enter_cwd(&mut self, dir: CwdPath) -> Cwd {
        std::mem::replace(&mut self.context.cwd, Cwd(dir))
    }

    /// Put back a cell [`Self::enter_cwd`] saved, undoing every `cd` since.
    pub(crate) fn restore_cwd(&mut self, saved: Cwd) {
        self.context.cwd = saved;
    }

    /// Move the logical cwd to `target`, resolved as every path builtin
    /// resolves (see [`Self::resolve`]).  A relative one folds lexically, so
    /// symlinks survive as under bash's default `cd -L`.
    ///
    /// # Errors
    /// If the resolved target cannot be stat'd, is not a directory, or is no
    /// working directory ([`CwdPath`]).
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:cwd-stat] `cd`: stats the resolved target to confirm it is a directory before updating the logical cwd; a directory-existence check, not turn-time model data I/O, raises no surface card."
    )]
    pub(crate) fn apply_chdir(&mut self, target: &str) -> Result<(), Error> {
        let resolved = self.resolve(target);
        let meta = std::fs::metadata(resolved.as_path())
            .map_err(|e| Error::new(format!("{}: {e}", resolved.shown())))?;
        if !meta.is_dir() {
            return Err(Error::new(format!("{}: not a directory", resolved.shown())));
        }
        let resolved = resolved.into_cwd().map_err(|e| Error::new(e.to_string()))?;
        self.context.cwd = Cwd(resolved);
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

    /// Absolute path of the executable the shell would run for `name`, via the
    /// effective `PATH` and cwd; `None` if there is none.
    ///
    /// A filesystem question only — admission is [`GrantStack::admits`](crate::capability::GrantStack::admits)
    /// (head alone) and [`Self::check_exec`] (full call).  `which` pairs
    /// the two to tell denied-but-installed from absent.
    pub(crate) fn locate_command(&self, name: &str) -> Option<PathBuf> {
        let env_path = self.context.env_overrides.get_or_host("PATH");
        let cwd = self.cwd();
        crate::path::locate(
            name,
            env_path.as_deref(),
            crate::path::SearchCwd::of(cwd.as_path()),
        )
    }
}
