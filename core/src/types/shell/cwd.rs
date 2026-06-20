//! Logical working directory and path-resolution verbs.
//!
//! The shell-owned [`Cwd`] pair on `context.cwd` is
//! the canonical "where are we" state; the process cwd is left alone
//! because spawned ral threads would race it (a `spawn` / `par` /
//! pipeline stage running in parallel would see a sibling's `cd` as a
//! sudden reorder).  Child processes still see the right directory:
//! [`crate::runtime::command::process::apply_env`] passes
//! [`Shell::cwd`] as `Command::current_dir` and writes the pair into
//! their env as `PWD` / `OLDPWD`.

use super::Shell;
use crate::path::process_cwd;
use crate::path::tilde::{TildePath, expand_tilde_path};
use crate::types::Error;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The shell-owned logical cwd pair.
///
/// `current` is the directory `cd` last moved to. The user-visible cwd;
/// what `Shell::cwd()` returns when no `within [dir: …]` override is
/// active.
///
/// `previous` is the directory `cd` last moved away from. The `OLDPWD`
/// companion; surfaced to child processes via `apply_env` and to ral code
/// via the `cd -` shorthand.
///
/// `current = None` means "uninitialised"; readers fall back through
/// [`process_cwd`]. Front ends call `Shell::seed_default_env_vars` at
/// startup, which seeds `current` from the process cwd and `previous`
/// from `$OLDPWD` if the launching shell exported one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cwd {
    pub current: Option<PathBuf>,
    pub previous: Option<PathBuf>,
}

impl Shell {
    /// Logical working directory of the shell.  Precedence:
    ///
    /// 1. `context.dir` — the `within [dir: …]` override, rolled back
    ///    on scope exit.
    /// 2. `context.cwd.current` — the shell-owned cwd `cd` mutates,
    ///    persistent across thunks and snapshotted into spawned threads.
    /// 3. [`process_cwd`] — the OS-level cwd, kept around as a fallback
    ///    for shells that have not been seeded yet (defaulted test
    ///    shells, the pipeline helper subprocess).
    /// 4. `"."` — last resort if even `getcwd(3)` fails.
    ///
    /// The `cwd` builtin and every path-resolving builtin route
    /// through this accessor so a `within` scope or a prior `cd` is
    /// visible to the whole interpreter, not just to spawned child
    /// processes.
    pub fn cwd(&self) -> PathBuf {
        if let Some(p) = self.mobile.context.cwd_chain() {
            return p.to_path_buf();
        }
        // Literal "." when no cwd resolves at all (no within
        // override, no `cd`-tracked cwd, no process cwd).  Pure type
        // lift, not path construction.
        #[allow(clippy::disallowed_methods)]
        let fallback = || PathBuf::from(".");
        process_cwd().unwrap_or_else(fallback)
    }

    /// Change the shell's logical working directory.  Mutates the
    /// shell-owned [`Cwd`](crate::types::Cwd) pair on `context.cwd`;
    /// does not touch the process cwd, which would race other ral
    /// threads.  Child processes inherit the new cwd via
    /// [`crate::runtime::command::process::apply_env`]'s
    /// `Command::current_dir(shell.cwd())`, which threads the same
    /// value uniformly regardless of `within [dir: …]`.
    ///
    /// `OLDPWD` / `PWD` similarly live on shell state, not process
    /// env: `apply_env` writes them on each spawn from
    /// `context.cwd.previous` and [`Self::cwd`].  Inside ral code,
    /// reads go through `cwd` — `$env[PWD]` is filtered out of `$env`
    /// at the source.
    ///
    /// `OLDPWD` is the *effective* cwd captured the instant before this
    /// `cd`: `old` reads through [`Self::cwd`], so a `within [dir: …]`
    /// override in force at the call counts as "where we were."  That is
    /// bash's `OLDPWD = $PWD-before-cd` contract, and `cd -` returns to
    /// exactly that directory.
    ///
    /// Tilde expansion is delegated to
    /// [`expand_tilde_path`]; an empty `target` is treated as `~`.
    /// Relative targets resolve against the current logical cwd
    /// (matching bash's default `cd -L`: symlinks are preserved; only
    /// `.` / `..` components are normalised).  Returns
    /// `(old_path, new_path)` so the caller can fire the `chpwd`
    /// hook.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:cwd-stat] `cd`: stats the resolved target to confirm it is a directory before updating the logical cwd; a directory-existence check, not turn-time model data I/O, raises no surface card."
    )]
    pub fn apply_chdir(&mut self, target: &str) -> Result<(String, String), Error> {
        let old = self.cwd();

        let home = self.mobile.context.home();
        let home = if home.is_empty() { ".".into() } else { home };
        let raw: String = if target.is_empty() {
            expand_tilde_path(None, None, &home)
        } else if let Some(path) = TildePath::parse(target) {
            expand_tilde_path(path.user.as_deref(), path.suffix.as_deref(), &home)
        } else {
            target.into()
        };

        // Anchor relative paths against the current logical cwd and
        // normalise `.` / `..` without touching the filesystem.
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

    /// Resolve `path` against the shell's effective cwd
    /// ([`Self::cwd`]), minting a [`ResolvedPath`].  Forwards through
    /// [`Context::resolver`](super::Context::resolver), so a
    /// `within [dir: …]` override is honoured first and a prior `cd`
    /// is honoured otherwise.  The fs gates consume this directly; a
    /// caller that opens the file calls `.into_inner()` / `.as_path()`.
    pub fn resolve(&self, path: &str) -> crate::path::ResolvedPath {
        self.mobile.context.resolver().resolve(path)
    }

    /// Locate `name` on disk via the shell's effective `PATH` and
    /// `cwd`.  Thin Shell-aware wrapper over
    /// [`crate::path::locate`]; returns the absolute path of the
    /// executable file the shell would run for `name`, or `None` if
    /// no such file exists.
    ///
    /// Pure filesystem question ("is this command installed?");
    /// admission against the active grant lives in
    /// [`crate::capability::admits_head`] (head-only)
    /// and [`Self::check_exec_args`] (full call).  Together they let
    /// `which` and the dispatch error path tell "denied but
    /// installed" apart from "not installed."
    pub fn locate_command(&self, name: &str) -> Option<PathBuf> {
        let env_path = self.mobile.context.env_overrides.get_or_host("PATH");
        let cwd = self.cwd();
        crate::path::locate(name, env_path.as_deref(), Some(cwd.as_path()))
    }
}
