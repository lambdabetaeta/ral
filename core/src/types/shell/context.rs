//! `impl Context`: verbs over the dynamic context — env overrides, `HOME` /
//! `USER`, and the [`Resolver`] bound to the live home/cwd pair.
//!
//! [`Context`] is the `Shell::context` field that [`Shell::child`](super::Shell)
//! clones into a child.  `PWD` stays out of
//! `env_overrides`; the canonical directory lives on `context.cwd`.

use super::Context;
use crate::path::{CwdPath, Resolver};
use crate::types::EnvVars;
use std::path::PathBuf;

impl Context {
    /// Read-only borrow; mutation goes through [`Self::set_env_var`] and friends.
    pub fn env_overrides(&self) -> &EnvVars {
        &self.env_overrides
    }

    /// Insert `k → v`.
    pub fn set_env_var(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.env_overrides.insert(k.into(), v.into());
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

    /// Effective `HOME`: [`EnvVars::home`](crate::types::EnvVars::home) over
    /// these overrides.
    pub(crate) fn home(&self) -> Option<String> {
        self.env_overrides.home()
    }

    /// [`Self::home`], or the one sentence that names an unset `HOME`: what
    /// `~` and `home` ask for, each showing it through [`crate::path`].
    ///
    /// # Errors
    /// `HOME` is unset.
    pub(crate) fn require_home(&self) -> Result<String, crate::types::Error> {
        self.home().ok_or_else(|| {
            crate::types::Error::new("HOME is unset, so `~` and `home` name no directory")
                .with_hint("set HOME, or spell out an explicit path")
        })
    }

    /// The `USER` stamped on observations.  Overrides only, with no
    /// host-env fallback, so it names nobody until a front end has run
    /// [`boot_shell`](crate::boot::boot_shell).
    /// An empty binding names nobody either.
    pub fn principal(&self) -> Option<String> {
        self.env_overrides
            .get("USER")
            .filter(|u| !u.is_empty())
            .cloned()
    }

    /// The cwd cell's directory.
    pub(crate) fn cwd(&self) -> &CwdPath {
        &self.cwd
    }

    /// Where a child launched from this context starts: the cell's directory.
    pub(crate) fn launch_cwd(&self) -> PathBuf {
        self.cwd().as_path().to_path_buf()
    }

    /// A [`Resolver`] bound to this layer's environment and cwd — grant-prefix
    /// resolution, deny-path canonicalisation, and the fs guards all mint one here.
    pub(crate) fn resolver(&self) -> Resolver<'_> {
        Resolver::at(&self.env_overrides, self.cwd())
    }
}

#[cfg(test)]
impl Context {
    /// A context under `grants` and nothing else, at the test process's cwd:
    /// a fixture for the decisions that read a context alone.
    pub(crate) fn under_grants(grants: crate::capability::GrantStack) -> Self {
        Self {
            env_overrides: EnvVars::default(),
            cwd: crate::test_helper::process_cwd(),
            grants,
            handlers: super::HandlerStack::default(),
            args: Vec::new(),
            modules: super::Modules::default(),
        }
    }
}

impl<H> Context<H> {
    /// The same context over other handlers.
    pub(crate) fn map_handlers<G>(self, f: impl FnOnce(H) -> G) -> Context<G> {
        let Self {
            env_overrides,
            cwd,
            grants,
            handlers,
            args,
            modules,
        } = self;
        Context {
            env_overrides,
            cwd,
            grants,
            handlers: f(handlers),
            args,
            modules,
        }
    }

    /// A copy of this context over `handlers`.
    pub(crate) fn with_handlers<G>(&self, handlers: G) -> Context<G> {
        Context {
            env_overrides: self.env_overrides.clone(),
            cwd: self.cwd.clone(),
            grants: self.grants.clone(),
            handlers,
            args: self.args.clone(),
            modules: self.modules.clone(),
        }
    }

    /// The same context over other handlers, if `f` can make them.
    pub(crate) fn try_map_handlers<G, E>(
        self,
        f: impl FnOnce(H) -> Result<G, E>,
    ) -> Result<Context<G>, E> {
        let Self {
            env_overrides,
            cwd,
            grants,
            handlers,
            args,
            modules,
        } = self;
        Ok(Context {
            env_overrides,
            cwd,
            grants,
            handlers: f(handlers)?,
            args,
            modules,
        })
    }
}
