//! Scope-guard combinators: every `with_*` and the alias-frame
//! lifecycle, plus the audit-subtree combinators.
//!
//! ral evaluation does not rely on Rust unwinding for control flow, so
//! each guard here is an inline save-modify-restore around the body
//! rather than an RAII type.  Two groups:
//!
//! - **Attenuation guards** (paired with `within` / `grant`):
//!   [`Shell::with_capabilities`], [`Shell::with_env`],
//!   [`Shell::with_cwd`], [`Shell::with_handlers`].  Each pushes onto
//!   (or substitutes into) the dynamic context, runs `f`, then
//!   restores.
//!
//! - **Alias management** ([`Shell::install_alias`],
//!   [`Shell::remove_alias`], [`Shell::has_alias`]): aliases share the
//!   handler stack with `within` but a different lifetime discipline
//!   — installed permanently, removed only by an explicit call.  Each
//!   alias is a one-entry frame with no catch-all, pushed via the
//!   shared [`HandlerStack::push`] + removed via
//!   [`HandlerStack::remove_alias`].
//!
//! [`Shell::audit_child`] and [`Shell::audit_forced_child`] are the
//! audit-tree counterpart: run `f` in a fresh subtree and return the
//! nodes it produced as an [`AuditFragment`].

use super::Shell;
use crate::types::{
    AuditFragment, Binding, BuiltinEntry, Capabilities, HandlerEntry, Settled, Value, sig,
};

impl Shell {
    /// Run `f` with `capabilities` pushed for its dynamic extent.
    /// The single gate for every entry into capability-checked code:
    /// user `grant { … }` blocks and plugin hook / keybinding / alias
    /// dispatch all funnel through here, so no one forgets to
    /// push/pop.  Pushed on top of the caller's stack, so effective
    /// authority is always caller ∩ this layer.
    pub fn with_capabilities<R>(
        &mut self,
        capabilities: Capabilities,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.mobile.context.grants.push(capabilities);
        let r = f(self);
        self.mobile.context.grants.pop();
        r
    }

    /// Push a capability frame at session start — no paired pop, the
    /// frame survives until process exit.  The session-wide ceiling
    /// applied by `ral --capabilities <file.ral>` lives here, sitting
    /// above the [`Capabilities::root`] frame [`Shell::new`] installs.
    /// Use [`Self::with_capabilities`] for scoped attenuation
    /// (`grant {}`) instead — its push/pop pair is the right shape
    /// when the frame's lifetime is lexical.
    pub fn push_session_capabilities(&mut self, capabilities: Capabilities) {
        self.mobile.context.grants.push(capabilities);
    }

    /// True when a non-root capabilities layer is active.
    pub fn has_active_capabilities(&self) -> bool {
        self.mobile.context.grants.is_restrictive()
    }

    /// Run `f` with `overrides` merged into the ambient environment.
    /// Pair of the `within [env: …]` keyword.  Restored on normal
    /// return; ral evaluation does not rely on Rust unwinding for
    /// control flow, so an inline save/restore is sufficient
    /// (cf. [`Self::run_with_mobile`]).
    pub fn with_env<R>(
        &mut self,
        overrides: std::collections::HashMap<String, String>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let saved = self.mobile.context.env_overrides.clone();
        self.mobile.context.extend_env(overrides);
        let result = f(self);
        self.mobile.context.env_overrides = saved;
        result
    }

    /// Run `f` with `cwd` set as the ambient working directory.  Pair
    /// of the `within [dir: …]` keyword.
    pub fn with_cwd<R>(&mut self, cwd: std::path::PathBuf, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.mobile.context.dir.replace(cwd);
        let result = f(self);
        self.mobile.context.dir = saved;
        result
    }

    /// Run `f` with a handler frame pushed onto the handler stack for
    /// its dynamic extent.  Pair of the `within [handlers: …, handler:
    /// …]` keywords.  Allocates a [`FrameHandle`] internally and uses it
    /// for the paired remove — callers do not need to track handles.
    pub fn with_handlers<R>(
        &mut self,
        entries: Vec<HandlerEntry>,
        catch_all: Option<Value>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let handle = self.mobile.context.handlers.push(entries, catch_all);
        let result = f(self);
        self.mobile.context.handlers.remove_by_handle(handle);
        result
    }

    /// Install `thunk` as the alias for `name`.  Replaces any existing
    /// alias for the same name.  Sibling of [`Self::with_handlers`]:
    /// same handler stack, different lifetime discipline — aliases are
    /// not popped at scope exit, only by [`Self::remove_alias`] (or
    /// `unalias NAME`).
    ///
    /// An alias is cross-turn dynamic rebinding, so the arm's scheme is
    /// computed and stored on the frame at install — by the same static
    /// inference a turn's check uses (one engine), seeded from the live
    /// scope and closed against its own unifier.  This covers all three
    /// install paths uniformly (the `alias` statement, rc `aliases:`
    /// maps, plugin loads).
    pub fn install_alias(&mut self, name: String, thunk: Value) -> Settled<()> {
        if self.mobile.scope.get(&name).is_some() {
            return Err(sig(format!(
                "alias: `{name}` is a lexical binding in this scope; aliases install handler entries"
            )));
        }
        if crate::builtins::is_builtin(&name) {
            return Err(sig(format!(
                "alias: cannot alias builtin `{name}`; lexical and builtin names are not handler names"
            )));
        }
        let arm = match &thunk {
            Value::Lambda { param, body, .. } => Some((Some(param), body)),
            Value::Block { body, .. } => Some((None, body)),
            _ => None,
        };
        let scheme = match arm {
            Some((param, body)) => Some(
                crate::typecheck::alias_arm_scheme(&name, param, body, self.session_schemes())
                    .map_err(|m| {
                        use crate::typecheck::fmt_mode;
                        sig(format!(
                            "alias: `{name}`'s body changes the head's pipeline mode \
                             ({} vs {}); an alias reinterprets a head and must preserve its \
                             modes — match the existing head's modes or add a codec",
                            fmt_mode(&m.left),
                            fmt_mode(&m.right)
                        ))
                    })?,
            ),
            None => None,
        };
        self.mobile.context.handlers.remove_alias(&name);
        let mut entry = HandlerEntry::ral_per_name(name, thunk);
        entry.scheme = scheme;
        self.mobile.context.handlers.push_alias(vec![entry]);
        Ok(())
    }

    /// Install `value` as a lexical scope binding for `name` (the rc
    /// `bindings:` path).  Where [`Self::install_alias`] pushes a handler
    /// frame, this writes a scope entry: for a callable the closed
    /// session scheme is inferred under the value/function-application
    /// convention and stored alongside the value, so the next turn's
    /// check sees its type and the binding is callable by function
    /// application at the prompt.  A non-callable carries no scheme.
    pub fn bind_value(&mut self, name: String, value: Value) {
        let arm = match &value {
            Value::Lambda { param, body, .. } => Some((Some(param), body)),
            Value::Block { body, .. } => Some((None, body)),
            _ => None,
        };
        let scheme = arm.map(|(param, body)| {
            crate::typecheck::binding_value_scheme(param, body, self.session_schemes())
        });
        self.mobile
            .scope
            .set_binding(name, Binding { value, scheme });
    }

    /// The next turn's check seed, read off the live session: every
    /// scope binding with its installed scheme, plus the alias arms'
    /// schemes off the persistent handler frames.
    pub fn session_schemes(&self) -> crate::typecheck::SessionSchemes {
        crate::typecheck::SessionSchemes {
            bindings: self.mobile.scope.binding_schemes(),
            aliases: self.mobile.context.handlers.alias_schemes(),
        }
    }

    /// Remove the alias for `name` if one is installed.  Returns
    /// whether anything was removed.
    pub fn remove_alias(&mut self, name: &str) -> bool {
        self.mobile.context.handlers.remove_alias(name).is_some()
    }

    /// True if a removable alias frame is currently installed for
    /// `name`. Scoped `within` handler frames are not aliases even when
    /// they have the same one-entry shape.
    pub fn has_alias(&self, name: &str) -> bool {
        self.mobile
            .context
            .handlers
            .iter()
            .any(|f| f.is_alias_for(name))
    }

    /// Install process-static builtin commands into this shell.
    pub fn install_builtins(&mut self, entries: &'static [BuiltinEntry]) {
        self.session.builtins.install_static(entries);
    }

    /// Install captured builtin commands into this shell.
    pub fn install_captured_builtins(&mut self, entries: std::sync::Arc<[BuiltinEntry]>) {
        self.session.builtins.install_arc(entries);
    }

    /// Look up the innermost handler entry for `name` across the
    /// handler stack.  Thin Shell-side accessor over
    /// [`HandlerStack::lookup`](crate::types::HandlerStack::lookup).
    pub fn lookup_handler(&self, name: &str) -> Option<(HandlerEntry, usize)> {
        self.mobile.context.handlers.lookup(name)
    }

    /// Look up a builtin command binding installed in this shell.
    pub fn lookup_builtin(&self, name: &str) -> Option<BuiltinEntry> {
        self.session.builtins.get(name)
    }

    /// Run `f` inside a fresh audit subtree, returning the body's
    /// result alongside the nodes it produced.  The shell's audit
    /// state is restored on return; the parent's trail is set aside
    /// and reinstalled, so children flow into a dedicated
    /// [`AuditFragment`] rather than landing directly in the parent's
    /// tree.
    ///
    /// When audit is inactive the body runs unchanged and the
    /// returned fragment is empty.  `try` and `audit` need to collect
    /// children even outside a surrounding `audit { … }` scope; they
    /// use [`Self::audit_forced_child`] instead.
    pub fn audit_child<F, R>(&mut self, f: F) -> (AuditFragment, R)
    where
        F: FnOnce(&mut Shell) -> R,
    {
        if !self.local.audit.active() {
            let result = f(self);
            return (AuditFragment::empty(), result);
        }
        let parent = self.local.audit.enter_child();
        let result = f(self);
        let fragment = self.local.audit.leave_child(parent);
        (fragment, result)
    }

    /// Forced variant of [`Self::audit_child`]: install a fresh
    /// subtree for `f` regardless of parent state.  Used by `try` (to
    /// find the failing child) and `audit` (to return the full
    /// subtree), both of which collect children even when no outer
    /// audit is active.
    pub fn audit_forced_child<F, R>(&mut self, f: F) -> (AuditFragment, R)
    where
        F: FnOnce(&mut Shell) -> R,
    {
        let parent = self.local.audit.enter_forced_child();
        let result = f(self);
        let fragment = self.local.audit.leave_child(parent);
        (fragment, result)
    }
}
