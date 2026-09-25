//! Logical working directory and path-resolution verbs.
//!
//! `cd` moves the shell-owned [`Cwd`] on `context.cwd`, never the process cwd:
//! that one is OS-global, and a parallel `spawn` / `par` / pipeline stage would
//! see a sibling's `cd` as a sudden reorder.  Children still land right —
//! `apply_env` in `core/src/runtime/command/process.rs` passes [`Shell::cwd`]
//! as `Command::current_dir` and exports it as `PWD`.

use super::Shell;
use crate::path::process_cwd;
use crate::path::tilde::{TildePath, expand_tilde_path};
use crate::types::Error;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The working directory, one state cell: `cd` sets it, [`Shell::cwd`] gets
/// it, and `within [dir: …]` is its local-state handler, restoring the cell on
/// every exit.
///
/// It keeps no previous directory, hence no `OLDPWD`.  `None` means unseeded — readers fall back through [`process_cwd`] until
/// [`Shell::seed_default_env_vars`] or [`Shell::seed_cwd`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cwd(pub(in crate::types::shell) Option<PathBuf>);

impl Shell {
    /// The logical cwd: the cell's directory, else [`process_cwd`] for an
    /// unseeded shell, else `"."` if even `getcwd(3)` fails.
    ///
    /// Every path-resolving builtin routes through here, so a `within` scope or
    /// a prior `cd` binds the whole interpreter, not just spawned children.
    pub fn cwd(&self) -> PathBuf {
        if let Some(p) = self.context.cwd() {
            return p.to_path_buf();
        }
        process_cwd().unwrap_or_else(|| PathBuf::from("."))
    }

    /// State the logical cwd outright, overriding whatever
    /// [`Shell::seed_default_env_vars`] adopted, for a host that never
    /// `chdir`s the process — exarch's `boot_root_shell` seats a session here.
    pub fn seed_cwd(&mut self, cwd: PathBuf) {
        self.context.cwd = Cwd(Some(cwd));
    }

    /// `within [dir: …]`'s entry: the cell becomes `dir`, and the prior cell
    /// comes back for [`Self::restore_cwd`].  Not a `cd`, so no `chdir`
    /// authority is asked.
    pub(crate) fn enter_cwd(&mut self, dir: PathBuf) -> Cwd {
        std::mem::replace(&mut self.context.cwd, Cwd(Some(dir)))
    }

    /// Put back a cell [`Self::enter_cwd`] saved, undoing every `cd` since.
    pub(crate) fn restore_cwd(&mut self, saved: Cwd) {
        self.context.cwd = saved;
    }

    /// Move the logical cwd to `target`, resolved against [`Self::cwd`].
    /// Empty `target` means `~`; relative ones fold lexically, so symlinks
    /// survive as under bash's default `cd -L`.
    ///
    /// # Errors
    /// If the resolved target cannot be stat'd, or is not a directory.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:cwd-stat] `cd`: stats the resolved target to confirm it is a directory before updating the logical cwd; a directory-existence check, not turn-time model data I/O, raises no surface card."
    )]
    pub(crate) fn apply_chdir(&mut self, target: &str) -> Result<(), Error> {
        let old = self.cwd();

        let home = self.context.home();
        let raw: String = if let Some(path) = TildePath::parse(target) {
            expand_tilde_path(
                path.user.as_deref(),
                path.suffix.as_deref(),
                home.as_deref(),
            )
            .map_err(|cause| Error::new(format!("{target}: {}", cause.why()), 1))?
        } else {
            target.into()
        };

        let resolved = crate::path::resolve_path(Some(&old), &raw);

        let meta = std::fs::metadata(&resolved)
            .map_err(|e| Error::new(format!("{}: {e}", resolved.display()), 1))?;
        if !meta.is_dir() {
            return Err(Error::new(
                format!("{}: not a directory", resolved.display()),
                1,
            ));
        }

        self.context.cwd = Cwd(Some(resolved));
        Ok(())
    }

    /// Resolve `path` against the effective cwd, minting a
    /// [`crate::path::ResolvedPath`] that the fs gates consume directly; a
    /// caller that opens the file takes `.into_inner()` / `.as_path()`.
    pub fn resolve(&self, path: &str) -> crate::path::ResolvedPath {
        self.context.resolver().resolve(path)
    }

    /// Absolute path of the executable the shell would run for `name`, via the
    /// effective `PATH` and cwd; `None` if there is none.
    ///
    /// A filesystem question only — admission is `capability::admits_head`
    /// (head alone) and [`Self::check_exec_args`] (full call).  `which` and the
    /// dispatch error path pair the two to tell denied-but-installed from absent.
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
