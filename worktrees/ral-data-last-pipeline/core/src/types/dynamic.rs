//! Dynamic context (σ).
//!
//! The components of shell state that flow with dynamic extent: shell
//! environment vars, current working directory, capability restriction
//! stack, effect-handler stack, and invocation positional args.  These
//! travel together as a unit through every same-thread thunk and every
//! thread spawn — the `inherit_from`/`spawn_thread` paths clone
//! `Dynamic` whole, and `return_to` drops it.
//!
//! Replaces the old `Ambient` struct plus the lifted `script_args`
//! flat field on `Shell`.  After this refactor, `Ambient` no longer
//! exists; everything dynamic-extent lives here.
//!
//! `script_args` is grouped here because it inherits with the caller
//! (positional arguments propagate from script to sourced module to
//! function call without rebinding).  Unlike `env_vars` / `cwd` /
//! `capabilities_stack` / `handler_stack`, `within` and `grant` do not
//! modify it — it's "dynamic" in the inherit-with-caller sense, not
//! the attenuable-by-`within` sense.
//!
//! Wire format: `Dynamic` is *not* `Serialize` / `Deserialize`.  The
//! sandbox IPC layer (`sandbox::ipc`) defines an `IpcAmbient` mirror
//! holding the four ambient sub-fields; `script_args` is packed as a
//! separate wire field.  Wire layout is preserved across this
//! refactor.

use crate::path::{process_cwd, CanonMode, Resolver};
use crate::types::{Capabilities, HandlerFrame};
use std::collections::HashMap;
use std::path::PathBuf;
use super::audit::{Audit, Location};
use super::error::EvalSignal;
use super::capability::SandboxProjection;

/// Dynamically-scoped runtime context.
#[derive(Debug, Clone, Default)]
pub struct Dynamic {
    /// Process environment overrides (`within [shell: ...]`).  The
    /// field is private so the only paths to mutation go through
    /// [`Self::set_env_var`] / [`Self::set_env_var_or_keep`] /
    /// [`Self::extend_env`], all of which guard against process-owned
    /// keys (`PWD`, `OLDPWD`).  A stored copy of those would go stale
    /// on the first `cd` inside a thunk, since `dynamic` rolls back on
    /// `return_to` while the process CWD does not.
    env_vars: HashMap<String, String>,
    /// Working directory override (`within [dir: ...]`).
    pub cwd: Option<PathBuf>,
    /// Capability restriction stack — innermost last.
    pub capabilities_stack: Vec<Capabilities>,
    /// `within [handlers: …, handler: …]` effect-handler stack —
    /// innermost last.
    pub handler_stack: Vec<HandlerFrame>,
    /// Invocation positional args (`$args`, `$1`, ...) passed on the
    /// command line or by `source`.  Inherited with caller; not
    /// modified by `within` / `grant`.
    pub script_args: Vec<String>,
}

// ── Capability policy ─────────────────────────────────────────────────────
//
// Capability checks live on `Dynamic` rather than on `Shell` so that the
// type system *prevents* policy code from reading lexical scope, REPL
// scratch, control state, or exit hints — the policy operates on the
// dynamic capability stack and emits into a separately-borrowed `Audit`,
// with diagnostic location passed as `&Location`.  `Shell::check_*` are
// thin shims that bind the right borrows.
//
// The decisions themselves are owned by `capability::EffectiveGrant`;
// each method below builds one and forwards.

impl Dynamic {
    /// Read-only borrow of the env-overrides map.  Callers iterate
    /// or look up by name; mutation goes through [`Self::set_env_var`]
    /// (and friends) so the PWD/OLDPWD guard cannot be bypassed.
    pub fn env_vars(&self) -> &HashMap<String, String> {
        &self.env_vars
    }

    /// Insert into `env_vars` with a debug-time guard against
    /// process-owned keys (`PWD`, `OLDPWD`).  Their canonical home is
    /// the process; a copy in `env_vars` would go stale on the first
    /// `cd` inside a thunk because `dynamic` rolls back on
    /// `return_to`.  `apply_chdir` writes them with `set_var`.
    pub fn set_env_var(&mut self, k: impl Into<String>, v: impl Into<String>) {
        let k = k.into();
        debug_assert!(
            !matches!(k.as_str(), "PWD" | "OLDPWD"),
            "{k} is process-owned; write to std::env, not Dynamic::env_vars",
        );
        self.env_vars.insert(k, v.into());
    }

    /// Insert into `env_vars` only if `k` is unset.  Same guard as
    /// [`Self::set_env_var`].  Mirrors `HashMap::entry().or_insert_with`.
    pub fn set_env_var_or_keep(&mut self, k: impl Into<String>, v: impl Into<String>) {
        let k = k.into();
        if !self.env_vars.contains_key(k.as_str()) {
            self.set_env_var(k, v);
        }
    }

    /// Bulk-insert into `env_vars`, guard-checked per item.
    pub fn extend_env<I, K, V>(&mut self, items: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in items {
            self.set_env_var(k, v);
        }
    }

    /// Wholesale replace `env_vars` with a vetted map.  `pub(crate)`
    /// so only `with_env_overrides`' save/restore and the sandbox
    /// IPC ambient install can use it; both have already validated
    /// the contents.
    pub(crate) fn replace_env_vars(&mut self, m: HashMap<String, String>) {
        self.env_vars = m;
    }

    /// Whether the active stack denies every candidate name outright.
    /// Used by `classify_command_head` to colour the dispatch site
    /// before any args are parsed.
    pub(crate) fn is_exec_denied_for(&self, names: &[&str]) -> bool {
        crate::capability::EffectiveGrant::from_dynamic(self).is_exec_denied_for(names)
    }

    /// True when capability checks should emit events into the exec
    /// tree.  Requires an active tree (`audit` or `ral --audit`) AND
    /// `audit: true` on at least one enclosing capabilities layer
    /// (SPEC §11.4-11.5).
    pub fn should_audit_capabilities(&self, audit: &Audit) -> bool {
        audit.tree.is_some() && self.capabilities_stack.iter().any(|ctx| ctx.audit)
    }

    /// Check that the `editor.read` capability is available.
    pub fn check_editor_read(&self, subcmd: &str) -> Result<(), EvalSignal> {
        crate::capability::EffectiveGrant::from_dynamic(self).check_editor_read(subcmd)
    }

    /// Check that the `editor.write` capability is available.
    pub fn check_editor_write(&self, subcmd: &str) -> Result<(), EvalSignal> {
        crate::capability::EffectiveGrant::from_dynamic(self).check_editor_write(subcmd)
    }

    /// Check that the `editor.tui` capability is available.
    pub fn check_editor_tui(&self) -> Result<(), EvalSignal> {
        crate::capability::EffectiveGrant::from_dynamic(self).check_editor_tui()
    }

    /// Check that the `shell.chdir` capability is available.
    pub fn check_shell_chdir(&self) -> Result<(), EvalSignal> {
        crate::capability::EffectiveGrant::from_dynamic(self).check_shell_chdir()
    }

    /// Effective `$HOME` at this dynamic layer.  Thin forwarder
    /// to [`crate::path::home::home`] — see that module for the
    /// resolution order.
    pub(crate) fn home(&self) -> String {
        crate::path::home::home(&self.env_vars)
    }

    /// Effective working directory: the dynamic override if any
    /// (`within [dir: …]`), else the process cwd, else `"."`.
    /// Used as the `cwd:` sigil base when freezing inline
    /// `grant {…}` policies — sigils must resolve against the
    /// shell's logical cwd, not the process's, so a `within`
    /// override is honoured.
    pub fn effective_cwd(&self) -> std::path::PathBuf {
        // No Shell at this layer — Dynamic is purer than Shell.
        self.cwd
            .clone()
            .or_else(process_cwd)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Build a [`Resolver`] tied to this dynamic layer.  Lenient
    /// canonicalisation: missing components fall back through the
    /// ancestor walk.  Use for grant prefix resolution, deny-path
    /// canonicalisation, and any check that runs outside a
    /// sandboxed child.
    pub(crate) fn resolver(&self) -> Resolver<'_> {
        Resolver {
            home: self.home(),
            cwd: self.cwd.as_deref(),
            mode: CanonMode::Lenient,
        }
    }

    /// Build a [`Resolver`] for an access-side capability check.
    /// Inside the sandboxed child the OS-level Seatbelt/bwrap
    /// profile is the real gate, and `realpath(3)` may fail on
    /// intermediate components or fall back to lexical form on
    /// only one side of the comparison; both lead to spurious
    /// denials.  We therefore use pure lexical resolution there,
    /// leaning on `path_within`'s firmlink-alias awareness to
    /// keep `/tmp` ↔ `/private/tmp` correct.  Outside the
    /// sandbox we keep canonicalise-based resolution so grants
    /// follow symlinks.
    pub(crate) fn resolver_for_check(&self) -> Resolver<'_> {
        let mode = if std::env::var_os(crate::sandbox::SANDBOX_ACTIVE_ENV).is_some() {
            CanonMode::LexicalOnly
        } else {
            CanonMode::Lenient
        };
        Resolver {
            home: self.home(),
            cwd: self.cwd.as_deref(),
            mode,
        }
    }

    /// Validate an `exec` capability check against the active stack and
    /// emit an audit node if auditing is on.
    pub fn check_exec_args(
        &self,
        display_name: &str,
        policy_names: &[&str],
        args: &[String],
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        crate::capability::EffectiveGrant::from_dynamic(self).check_exec_args(
            display_name,
            policy_names,
            args,
            audit,
            location,
        )
    }

    pub fn check_fs_read(
        &self,
        path: &str,
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        crate::capability::EffectiveGrant::from_dynamic(self).check_fs_read(path, audit, location)
    }

    pub fn check_fs_write(
        &self,
        path: &str,
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        crate::capability::EffectiveGrant::from_dynamic(self).check_fs_write(path, audit, location)
    }

    /// Compute the OS-renderable projection of the current capabilities
    /// stack, intersecting fs prefixes and ANDing net booleans across
    /// layers.  `deny_paths` accumulate as a union: more denies = less
    /// authority, monotone with stack depth.
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        crate::capability::EffectiveGrant::from_dynamic(self).sandbox_projection()
    }
}
