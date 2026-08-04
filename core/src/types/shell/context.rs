//! `impl Context`: verbs over the dynamic context — env overrides, `$HOME` /
//! `$USER`, the audit gate, and the [`Resolver`] bound to the live home/cwd pair.
//!
//! [`Context`] is the `mobile.context` subtree that `Shell::inherit_from` and
//! `Shell::spawn_thread` clone into a child.  `PWD` / `OLDPWD` stay out of
//! `env_overrides`; the canonical pair lives on `context.cwd`.

use super::Context;
use super::cwd::Cwd;
use crate::path::{Resolver, SearchCwd};
use crate::types::{Audit, EnvVars, GrantStack, HandlerStack, Modules};
use std::path::{Path, PathBuf};

impl Context {
    /// Read-only borrow; mutation goes through [`Self::set_env_var`] and friends.
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

    /// True when capability checks should emit an observation: an active
    /// trail (`audit { … }` or `ral --audit`) and `audit: true` on some
    /// grants layer.
    pub fn should_audit_capabilities(&self, audit: &Audit) -> bool {
        audit.active() && self.grants.any_audits()
    }

    /// Effective `$HOME` via [`crate::path::home`]: these overrides first, then
    /// the host env, empty string when neither binds.
    pub(crate) fn home(&self) -> String {
        crate::path::home(&self.env_overrides)
    }

    /// The `$USER` stamped on observations.  Overrides only, with no
    /// host-env fallback, so it stays empty until a front end has run
    /// [`Shell::seed_default_env_vars`](super::Shell::seed_default_env_vars).
    pub fn principal(&self) -> String {
        self.env_overrides.get("USER").cloned().unwrap_or_default()
    }

    /// Effective cwd: the `within [dir: …]` override, else the `cd`-mutated
    /// persistent cwd.  Sole home of that precedence, read by [`Self::resolver`]
    /// and [`Shell::cwd`](super::Shell::cwd), which adds the process-cwd fallback.
    pub(crate) fn cwd_chain(&self) -> Option<&Path> {
        self.dir.as_deref().or(self.cwd.current.as_deref())
    }

    /// The anchor a `PATH` walk made from this context runs against.
    ///
    /// The effective cwd is a precedence with exactly one home,
    /// [`Self::cwd_chain`]; a walk that re-derives it from `self.dir` alone
    /// anchors relative entries to nothing in a plain REPL and disagrees with
    /// every other consumer of "here" — which is how a walk and its 126/127
    /// probe once told different stories about the same name.
    pub(crate) fn search_cwd(&self) -> SearchCwd<'_> {
        self.cwd_chain()
            .map_or_else(SearchCwd::nowhere, SearchCwd::of)
    }

    /// A [`Resolver`] bound to this layer's home and effective cwd — grant-prefix
    /// resolution, deny-path canonicalisation, and the fs gates all mint one here.
    pub(crate) fn resolver(&self) -> Resolver<'_> {
        Resolver {
            home: self.home(),
            cwd: self.cwd_chain(),
        }
    }

    /// The raw `(dir, cwd)` pair for the wire mirror, which must carry the
    /// override and the `cd` slot separately; every cwd *consumer* reads
    /// [`Self::cwd_chain`] instead.
    pub(crate) fn wire_cwd_parts(&self) -> (Option<&Path>, &Cwd) {
        (self.dir.as_deref(), &self.cwd)
    }

    /// Rebuild a context from its wire mirror's parts — `crate::subprocess`
    /// is the sole caller.  `hooks` starts empty: host lifecycle entry points
    /// never ride the wire.
    pub(crate) fn from_wire(
        env_overrides: EnvVars,
        dir: Option<PathBuf>,
        grants: GrantStack,
        handlers: HandlerStack,
        args: Vec<String>,
        modules: Modules,
        cwd: Cwd,
    ) -> Self {
        Self {
            env_overrides,
            dir,
            grants,
            handlers,
            hooks: std::collections::HashMap::default(),
            args,
            modules,
            cwd,
        }
    }
}
