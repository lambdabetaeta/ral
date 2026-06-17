//! `impl Context`: dynamic-context verbs over the capability-attenuable
//! subtree of [`Mobile`](super::Mobile).
//!
//! [`Context`] lives as `mobile.context` and is what
//! [`Shell::inherit_from`](super::Shell::inherit_from) and
//! [`Shell::spawn_thread`](super::Shell::spawn_thread) clone into
//! children.  The methods here are its own verbs: dynamic env-override
//! reads/writes (`PWD` / `OLDPWD` live on `context.cwd`, not in
//! `env_overrides`); `$HOME` and `$USER` lookups
//! that pin "which env var, what fallback" in one place; the
//! audit-gating predicate; and the [`Resolver`] constructors that bind
//! the active cwd / home pair.

use super::Context;
use crate::path::{CanonMode, Resolver};
use crate::types::{Audit, EnvVars};
use std::path::Path;

impl Context {
    /// Read-only borrow of the env-overrides map.  Callers iterate or
    /// look up by name; mutation goes through [`Self::set_env_var`]
    /// (and friends).
    pub fn env_overrides(&self) -> &EnvVars {
        &self.env_overrides
    }

    /// Insert `k → v`.
    pub fn set_env_var(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.env_overrides.insert(k.into(), v.into());
    }

    /// Insert `k → v` only if `k` is unbound.
    pub fn set_env_var_or_keep(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.env_overrides.insert_or_keep(k.into(), v.into());
    }

    /// Bulk-insert each item.
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

    /// True when capability checks should emit nodes into the audit
    /// tree.  Requires both an active trail (`audit { … }` or
    /// `ral --audit`) and `audit: true` on at least one enclosing
    /// grants layer (SPEC §11.4–11.5).
    pub fn should_audit_capabilities(&self, audit: &Audit) -> bool {
        audit.active() && self.grants.any_audits()
    }

    /// Effective `$HOME` at this dynamic layer.  Forwards to
    /// [`crate::path::home`] — see [`crate::path::tilde`] for the
    /// resolution order.
    pub(crate) fn home(&self) -> String {
        crate::path::home(&self.env_overrides)
    }

    /// `$USER` from the dynamic env, empty string if unset.  The
    /// principal recorded on audit-tree nodes — `audit::*` builders
    /// and the capability-check emitter all read it here so the
    /// "which env var, what fallback" decision lives in one place.
    pub fn principal(&self) -> String {
        self.env_overrides.get("USER").cloned().unwrap_or_default()
    }

    /// Effective shell cwd as a borrow: the `within [dir: …]`
    /// override if any, else the `cd`-mutated persistent cwd, else
    /// `None` (the caller falls back through `process_cwd`).
    ///
    /// Single source of truth for the `dir > cwd.current` precedence
    /// rule.  [`Self::resolver`], [`Self::resolver_for_check`], and
    /// [`Shell::cwd`](super::Shell::cwd) all consume this — keeping
    /// the rule in one place means a future tweak (a third source, a
    /// different policy) ports once.
    pub(crate) fn cwd_chain(&self) -> Option<&Path> {
        self.dir.as_deref().or(self.cwd.current.as_deref())
    }

    /// Build a [`Resolver`] tied to this dynamic layer.  Lenient
    /// canonicalisation: missing components fall back through the
    /// ancestor walk.  Used for grant-prefix resolution, deny-path
    /// canonicalisation, and any check that runs outside a sandboxed
    /// child.
    pub(crate) fn resolver(&self) -> Resolver<'_> {
        self.resolver_with(CanonMode::Lenient)
    }

    /// Build a [`Resolver`] for an access-side capability check.
    ///
    /// The interpreter always runs unconfined now (a `grant` body
    /// evaluates locally; only the per-command launcher enters the OS
    /// sandbox, in a separate child process), so the in-process fs gate
    /// uses canonicalise-based resolution unconditionally and grants
    /// follow symlinks.
    pub(crate) fn resolver_for_check(&self) -> Resolver<'_> {
        self.resolver_with(CanonMode::Lenient)
    }

    fn resolver_with(&self, mode: CanonMode) -> Resolver<'_> {
        Resolver {
            home: self.home(),
            cwd: self.cwd_chain(),
            mode,
        }
    }
}
