//! Logical working directory and path-resolution verbs.
//!
//! `cd` moves the shell-owned [`Cwd`] on `context.cwd`, never the process cwd:
//! that one is OS-global, and a parallel `spawn` / `par` / pipeline stage would
//! see a sibling's `cd` as a sudden reorder.  Children still land right —
//! `apply_env` in `core/src/runtime/command/process.rs` passes [`Shell::cwd`]
//! as `Command::current_dir` and exports the pair as `PWD` / `OLDPWD`.

use super::Shell;
use crate::path::process_cwd;
use crate::path::tilde::{TildePath, expand_tilde_path};
use crate::types::Error;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The shell-owned logical cwd pair: where `cd` last moved to, and from.
///
/// `previous` exists only to export `OLDPWD` to children; ral has no `cd -` of
/// its own.  `current = None` means unseeded — readers fall back through
/// [`process_cwd`] until [`Shell::seed_default_env_vars`] or [`Shell::seed_cwd`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cwd {
    pub current: Option<PathBuf>,
    pub previous: Option<PathBuf>,
}

impl Shell {
    /// Effective logical cwd: the `within [dir: …]` override, else the
    /// `cd`-tracked `cwd.current`, else [`process_cwd`] for an unseeded shell,
    /// else `"."` if even `getcwd(3)` fails.
    ///
    /// Every path-resolving builtin routes through here, so a `within` scope or
    /// a prior `cd` binds the whole interpreter, not just spawned children.
    pub fn cwd(&self) -> PathBuf {
        if let Some(p) = self.mobile.context.cwd_chain() {
            return p.to_path_buf();
        }
        process_cwd().unwrap_or_else(|| PathBuf::from("."))
    }

    /// State the logical cwd outright, overriding whatever
    /// [`Shell::seed_default_env_vars`] adopted, for a host that never
    /// `chdir`s the process — exarch's `boot_root_shell` seats a session here.
    pub fn seed_cwd(&mut self, cwd: PathBuf) {
        self.mobile.context.cwd.current = Some(cwd);
    }

    /// Move the logical cwd to `target`, recording the prior effective cwd —
    /// read through [`Self::cwd`], so a `within [dir: …]` override counts as
    /// where we were — as `previous`.  Empty `target` means `~`; relative ones
    /// fold lexically, so symlinks survive as under bash's default `cd -L`.
    /// Returns `(old, new)` for the caller's `chpwd` hook.
    ///
    /// # Errors
    /// If the resolved target cannot be stat'd, or is not a directory.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:cwd-stat] `cd`: stats the resolved target to confirm it is a directory before updating the logical cwd; a directory-existence check, not turn-time model data I/O, raises no surface card."
    )]
    pub fn apply_chdir(&mut self, target: &str) -> Result<(String, String), Error> {
        let old = self.cwd();

        let home = self.mobile.context.home();
        let home = if home.is_empty() { ".".into() } else { home };
        let raw: String = if target.is_empty() {
            home
        } else if let Some(path) = TildePath::parse(target) {
            expand_tilde_path(path.user.as_deref(), path.suffix.as_deref(), &home).ok_or_else(
                || {
                    Error::new(
                        format!(
                            "{target}: cannot resolve another user's home directory on \
                             this platform (no getpwnam(3) equivalent)"
                        ),
                        1,
                    )
                },
            )?
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

        let old_str = old.to_string_lossy().into_owned();
        let new_str = resolved.to_string_lossy().into_owned();
        self.mobile.context.cwd.previous = Some(old);
        self.mobile.context.cwd.current = Some(resolved);
        self.mobile.control.last_status = 0;

        Ok((old_str, new_str))
    }

    /// Resolve `path` against the effective cwd, minting a
    /// [`crate::path::ResolvedPath`] that the fs gates consume directly; a
    /// caller that opens the file takes `.into_inner()` / `.as_path()`.
    pub fn resolve(&self, path: &str) -> crate::path::ResolvedPath {
        self.mobile.context.resolver().resolve(path)
    }

    /// Absolute path of the executable the shell would run for `name`, via the
    /// effective `PATH` and cwd; `None` if there is none.
    ///
    /// A filesystem question only — admission is `capability::admits_head`
    /// (head alone) and [`Self::check_exec_args`] (full call).  `which` and the
    /// dispatch error path pair the two to tell denied-but-installed from absent.
    pub fn locate_command(&self, name: &str) -> Option<PathBuf> {
        let env_path = self.mobile.context.env_overrides.get_or_host("PATH");
        let cwd = self.cwd();
        crate::path::locate(
            name,
            env_path.as_deref(),
            crate::path::SearchCwd::of(cwd.as_path()),
        )
    }
}
