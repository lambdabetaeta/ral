//! `impl Context`: verbs over the dynamic context — env overrides, `$HOME` /
//! `$USER`, and the [`Resolver`] bound to the live home/cwd pair.
//!
//! [`Context`] is the `Shell::context` field that `Shell::inherit_from` and
//! `Shell::spawn_thread` clone into a child.  `PWD` stays out of
//! `env_overrides`; the canonical directory lives on `context.cwd`.

use super::Context;
use super::cwd::Cwd;
use crate::path::{Resolver, SearchCwd};
use crate::types::{EnvVars, GrantStack, HandlerStack, Modules};
use std::path::Path;

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
    pub(crate) fn set_env_var_or_keep(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.env_overrides.insert_or_keep(k.into(), v.into());
    }

    /// Bulk-insert each item.
    pub(crate) fn extend_env<I, K, V>(&mut self, items: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in items {
            self.set_env_var(k, v);
        }
    }

    /// Effective `$HOME` via [`crate::path::home`]: these overrides first, then
    /// the host env, `None` when neither binds.
    pub(crate) fn home(&self) -> Option<String> {
        crate::path::home(&self.env_overrides)
    }

    /// The `$USER` stamped on observations.  Overrides only, with no
    /// host-env fallback, so it names nobody until a front end has run
    /// [`Shell::seed_default_env_vars`](super::Shell::seed_default_env_vars).
    /// An empty binding names nobody either.
    pub fn principal(&self) -> Option<String> {
        self.env_overrides
            .get("USER")
            .filter(|u| !u.is_empty())
            .cloned()
    }

    /// The cwd cell's directory, `None` while unseeded;
    /// [`Shell::cwd`](super::Shell::cwd) adds the process-cwd fallback.
    pub(crate) fn cwd(&self) -> Option<&Path> {
        self.cwd.0.as_deref()
    }

    /// The anchor a `PATH` walk made from this context runs against: the
    /// [`Self::cwd`] every other consumer of "here" reads.
    pub(crate) fn search_cwd(&self) -> SearchCwd<'_> {
        self.cwd().map_or_else(SearchCwd::nowhere, SearchCwd::of)
    }

    /// A [`Resolver`] bound to this layer's home and cwd — grant-prefix
    /// resolution, deny-path canonicalisation, and the fs gates all mint one here.
    pub(crate) fn resolver(&self) -> Resolver<'_> {
        Resolver {
            home: self.home(),
            cwd: self.cwd(),
        }
    }

    /// The cwd cell, for the wire mirror.
    pub(crate) fn wire_cwd(&self) -> &Cwd {
        &self.cwd
    }

    /// Rebuild a context from its wire mirror's parts — `crate::subprocess`
    /// is the sole caller.  `hooks` starts empty: host lifecycle entry points
    /// never ride the wire.
    pub(crate) fn from_wire(
        env_overrides: EnvVars,
        grants: GrantStack,
        handlers: HandlerStack,
        args: Vec<String>,
        modules: Modules,
        cwd: Cwd,
    ) -> Self {
        Self {
            env_overrides,
            grants,
            handlers,
            hooks: std::collections::HashMap::default(),
            args,
            modules,
            cwd,
        }
    }
}
