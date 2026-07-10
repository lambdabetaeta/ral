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
    AuditFragment, Binding, BuiltinEntry, Capabilities, HandlerEntry, HandlerRole, Settled, Value,
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
    /// …]` keywords.  Allocates a [`crate::types::FrameHandle`] internally and uses it
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
        let entry = HandlerEntry::vet(name, thunk, self.session_schemes(), HandlerRole::Alias)?;
        self.mobile.context.handlers.remove_alias(&entry.name);
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

    /// Bind `name` → `value` as a plain scope variable, inferring no
    /// scheme.  The scheme-less sibling of [`Self::bind_value`]: where
    /// `bind_value` types a definition so it is callable by function
    /// application at the prompt, this installs data (an env mirror like
    /// `USER` / `CWD`, the `RAL_PROMPT` thunk read by the renderer, a host
    /// seed var) that is *resolved* but never reinterpreted as a typed
    /// prompt-callable.  Keeping the two verbs distinct preserves the
    /// boundary an rc draws between its `bindings:` (typed) and its
    /// `env:` / `prompt:` (untyped) keys.
    pub fn set_var(&mut self, name: String, value: Value) {
        self.mobile.scope.set(name, value);
    }

    /// The evaluator's single install point for a scope entry
    /// (`decisions/260629_agent-binding-reaping`). A session-scope install
    /// (`Env::at_session_scope`) additionally stamps this shell's
    /// binding-lease ledger: a fresh non-baseline name starts its lease, a
    /// rebind renews it — writing a name is itself interest in it. Deeper
    /// installs (block/lambda/letrec frames, a `use` body) are recorded
    /// nowhere: the predicate is the classifier, not a caller obligation, so
    /// a pushed fixpoint frame needs no special case here. Every persistent
    /// top-level write routes through this verb — `assign_pattern`'s `Name`
    /// and `...rest` arms, and `eval_letrec`'s two installs — so "write a
    /// scope entry" and "stamp the ledger" can never be pulled apart at a
    /// call site. Host verbs ([`Self::bind_value`], [`Self::set_var`]) stay
    /// on the raw `Env` primitive: every host call to them precedes arming,
    /// so the boot baseline covers them without a special case.
    ///
    /// When armed and at session scope, also runs the large-binding check:
    /// if `binding.value`'s [`Value::shallow_size`] meets
    /// [`large_binding_bytes`](super::bindings::BindingLease::large_binding_bytes),
    /// queues a `LargeBindingNotice` — a residency nudge independent of
    /// baseline status and of the idle-lease check above, since a
    /// boot-seeded name can be just as large as a model-scratch one. A
    /// rebind that still meets the threshold queues another notice.
    pub(crate) fn install_scope_binding(&mut self, name: String, binding: Binding) {
        let session = self.mobile.scope.at_session_scope();
        if session {
            self.local.bindings.note_install(&name);
            if let Some(lease) = self.local.bindings.lease() {
                let bytes = binding.value.shallow_size() as u64;
                if bytes >= lease.large_binding_bytes {
                    self.local
                        .bindings
                        .queue_large_binding_notice(name.clone(), bytes);
                }
            }
        }
        self.mobile.scope.set_binding(name, binding);
    }

    /// Look `name` up in the lexical scope chain alone — *not* the
    /// pseudo-variable or builtin namespaces that [`Self::lookup_value_name`]
    /// also consults.  The read dual of [`Self::set_var`] /
    /// [`Self::bind_value`]: a host asking "is this name a lexical binding,
    /// and what does it hold" (prompt lookup, alias-conflict checks,
    /// worksheet projections) wants exactly this, so that a name shadowed by
    /// a builtin still reads as unbound in scope.
    pub fn scope_lookup(&self, name: &str) -> Option<&Value> {
        self.mobile.scope.get(name)
    }

    /// Every lexical binding's `(name, value)` across the whole scope
    /// chain, innermost shadowing outermost.  The enumeration a host
    /// drives tab-completion and the worksheet from; the read-many dual of
    /// [`Self::set_var`].
    pub fn bindings(&self) -> Vec<(String, Value)> {
        self.mobile.scope.all_bindings()
    }

    /// Every bound name with its installed scheme, innermost binding
    /// wins — the scope half of [`Self::session_schemes`], surfaced on its
    /// own for the worksheet's type column.
    pub fn binding_schemes(&self) -> Vec<(String, Option<crate::typecheck::Scheme>)> {
        self.mobile.scope.binding_schemes()
    }

    /// The type scheme bound to `name`, if it is a lexical binding that
    /// carries one.  Flattens "unbound" and "bound but scheme-less" to
    /// `None`: a name appears with a scheme only when [`Self::bind_value`]
    /// (or the prelude harvest) inferred one — pattern components and
    /// [`Self::set_var`] data bindings carry none.  The single-name
    /// companion of [`Self::binding_schemes`].
    pub fn binding_scheme(&self, name: &str) -> Option<&crate::typecheck::Scheme> {
        self.mobile.scope.get_binding(name)?.scheme.as_ref()
    }

    /// The names of every installed handler entry — `within` arms and
    /// aliases alike — for tab completion.  The handler-stack counterpart
    /// of [`Self::builtin_names`](Self::builtin_names).
    pub fn handler_names(&self) -> impl Iterator<Item = &str> {
        self.mobile
            .context
            .handlers
            .entries()
            .map(|e| e.name.as_ref())
    }

    /// Run `f` under a fresh innermost lexical scope, popped on return.
    /// The isolation primitive `use` and the REPL plugin loader share:
    /// a loaded file's top-level helper bindings live in this frame and
    /// are discarded when it pops, so they never leak into the caller's
    /// scope.  Pairs with [`crate::builtins::modules::evaluate_source`],
    /// which owns the cycle/depth guards and script-context swap; this
    /// owns only the scope frame.
    pub fn in_fresh_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.mobile.scope.push_scope();
        let r = f(self);
        self.mobile.scope.pop_scope();
        r
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
        F: FnOnce(&mut Self) -> R,
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
        F: FnOnce(&mut Self) -> R,
    {
        let parent = self.local.audit.enter_forced_child();
        let result = f(self);
        let fragment = self.local.audit.leave_child(parent);
        (fragment, result)
    }
}
