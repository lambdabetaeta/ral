//! The `with_*` scope guards and the alias-frame lifecycle.
//!
//! ral evaluation never unwinds for control flow, so each guard here is an
//! inline save-modify-restore around the body rather than an RAII type.

use super::Shell;
use crate::types::{
    Binding, Capabilities, Decision, HandlerEntry, HandlerRole, Map, Observation, Observed,
    Settled, Value,
};

impl Shell {
    /// Run `f` with `capabilities` pushed for its dynamic extent.  The single
    /// gate into capability-checked code — `grant { … }` blocks and plugin
    /// hook / keybinding / alias dispatch all funnel through here.  The push
    /// sits on top of the caller's stack, so effective authority is always
    /// caller ∩ this layer.
    pub fn with_capabilities<R>(
        &mut self,
        capabilities: Capabilities,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.mobile.context.grants.push(capabilities);
        self.audit_deputy_prefixes();
        let r = f(self);
        self.mobile.context.grants.pop();
        r
    }

    /// Push a capability frame with no paired pop: it survives to process
    /// exit.  Where `ral --capabilities <file.ral>`'s session-wide ceiling
    /// lands, above the [`Capabilities::root`] frame [`Shell::new`] installs.
    /// Lexical attenuation (`grant {}`) wants [`Self::with_capabilities`].
    pub fn push_session_capabilities(&mut self, capabilities: Capabilities) {
        self.mobile.context.grants.push(capabilities);
        self.audit_deputy_prefixes();
    }

    /// Observe a `deputy` capability check when the stack just pushed,
    /// meet-folded, is a confused deputy.
    /// [`crate::capability::deputy_prefixes`] demands the fold and only
    /// reports, never denies; this is its one call site.  No-op unless audit
    /// is live and some layer opted into capability auditing.
    pub(crate) fn audit_deputy_prefixes(&mut self) {
        if !self
            .mobile
            .context
            .should_audit_capabilities(&self.local.audit)
        {
            return;
        }
        let Some(folded) = self
            .mobile
            .context
            .grants
            .iter()
            .cloned()
            .reduce(Capabilities::meet)
        else {
            return;
        };
        let prefixes = crate::capability::deputy_prefixes(&folded);
        if prefixes.is_empty() {
            return;
        }
        let site = self.call_site();
        let principal = self.mobile.context.principal();
        let mut fields = Map::new();
        fields.insert(
            "prefixes".into(),
            Value::list(
                prefixes
                    .iter()
                    .map(|p| Value::String(p.as_str().to_string()))
                    .collect(),
            ),
        );
        // Pushed rather than sent through `evaluator::audit::observe_stamped`: a
        // `Flagged` decision has no rail branch in the policy table, so the
        // trail is the whole of this observation's audience and no `Mooring`
        // needs reaching this deep into the scope guards.
        self.local.audit.push(Observation::instant(
            site,
            principal,
            Observed::Capability {
                resource: "deputy".into(),
                decision: Decision::Flagged,
                fields,
            },
        ));
    }

    /// True when a non-root capabilities layer is active.
    pub fn has_active_capabilities(&self) -> bool {
        self.mobile.context.grants.is_restrictive()
    }

    /// Run `f` with `overrides` merged into the ambient environment — the
    /// `within [env: …]` pair.
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

    /// Run `f` with `cwd` as the ambient working directory — the
    /// `within [dir: …]` pair.
    pub fn with_cwd<R>(&mut self, cwd: std::path::PathBuf, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.swap_cwd_override(cwd);
        let result = f(self);
        self.restore_cwd_override(saved);
        result
    }

    /// Install `cwd` as the `within [dir: …]` override, returning the value
    /// it displaces.  `dir` is private to this module, so `WithinUndo`
    /// (`evaluator::scope`) reaches it through this pair rather than the
    /// field directly.
    pub(crate) fn swap_cwd_override(&mut self, cwd: std::path::PathBuf) -> Option<std::path::PathBuf> {
        self.mobile.context.dir.replace(cwd)
    }

    /// Restore a `within [dir: …]` override saved by [`Self::swap_cwd_override`].
    pub(crate) fn restore_cwd_override(&mut self, saved: Option<std::path::PathBuf>) {
        self.mobile.context.dir = saved;
    }

    /// Run `f` with a handler frame pushed for its dynamic extent — the
    /// `within [handlers: …, handler: …]` pair.  The frame's handle is minted
    /// and spent here, so callers never track one.
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

    /// Install `thunk` as the alias for `name`, replacing any existing one.
    /// Sibling of [`Self::with_handlers`]: same handler stack, but an alias
    /// frame is never popped at scope exit, only by [`Self::remove_alias`].
    /// Since it outlives its installing run, [`HandlerEntry::vet`] closes the
    /// arm's scheme against the live session and stores it on the frame to
    /// seed the next run's check.
    ///
    /// # Errors
    /// `thunk` not a unary lambda, or its body changing the head's pipeline
    /// mode.
    pub fn install_alias(&mut self, name: String, thunk: Value) -> Settled<()> {
        let entry = HandlerEntry::vet(name, thunk, self.session_schemes(), HandlerRole::Alias)?;
        self.mobile.context.handlers.remove_alias(&entry.name);
        self.mobile.context.handlers.push_alias(vec![entry]);
        Ok(())
    }

    /// Install `value` as a lexical scope binding for `name` — the rc
    /// `bindings:` path.  Where [`Self::install_alias`] pushes a handler
    /// frame, this writes a scope entry: a function value gets its closed
    /// session scheme inferred and stored alongside, so the next run's check
    /// sees a type and the name is applyable at the prompt.  Other values
    /// carry no scheme.
    pub fn bind_value(&mut self, name: String, value: Value) {
        let arm = match &value {
            Value::Thunk(closure) => match closure.comp.arrow() {
                Some((param, body)) => Some((Some(param), body)),
                None => Some((None, &closure.comp)),
            },
            _ => None,
        };
        let scheme = arm.map(|(param, body)| {
            crate::typecheck::binding_value_scheme(param, body, self.session_schemes())
        });
        self.mobile.scope.bind(name, Binding { value, scheme });
    }

    /// Bind `name` → `value` as a plain scope variable, inferring no scheme.
    /// The scheme-less sibling of [`Self::bind_value`], for data that is
    /// *resolved* but never applied — env mirrors like `USER` / `CWD`, the
    /// `RAL_PROMPT` thunk the renderer reads, host seed vars.  The two verbs
    /// stay distinct to hold the line an rc draws between its typed
    /// `bindings:` and its untyped `env:` / `prompt:` keys.
    pub fn set_var(&mut self, name: String, value: Value) {
        self.mobile.scope.bind(name, Binding { value, scheme: None });
    }

    /// The evaluator's single install point for a scope entry.  Lease
    /// bookkeeping lives in [`Self::note_define`] alone, called from
    /// `run_phrases`'s `Define` arm under `Mode::Session` — a nested
    /// `Bind`'s pattern is a local lexical name, never a session write, so
    /// it installs unleased here.
    pub(crate) fn install_scope_binding(&mut self, name: String, binding: Binding) {
        self.mobile.scope.bind(name, binding);
    }

    /// Record a `Phrase::Define`'s binding on the lease ledger: starts a
    /// fresh non-baseline name's lease and renews an existing one — writing
    /// a name is itself interest in it — and, when the lease is armed and
    /// [`Value::shallow_size`] meets `large_binding_bytes`, queues a
    /// residency notice.  Called from `run_phrases`'s `Define` arm alone,
    /// under `Mode::Session`.
    pub(crate) fn note_define(&mut self, name: &str, binding: &Binding) {
        self.local.bindings.note_install(name);
        if let Some(lease) = self.local.bindings.lease() {
            let bytes = binding.value.shallow_size() as u64;
            if bytes >= lease.large_binding_bytes {
                self.local
                    .bindings
                    .queue_large_binding_notice(name.to_string(), bytes);
            }
        }
    }

    /// Look `name` up in the lexical scope chain, natives included — *not*
    /// the pseudo-variable namespace [`Self::lookup_value_name`] also
    /// consults.  The read dual of [`Self::set_var`] / [`Self::bind_value`].
    pub fn scope_lookup(&self, name: &str) -> Option<&Value> {
        self.mobile.scope.get(name)
    }

    /// Every lexical binding across the whole scope chain, innermost
    /// shadowing outermost — what a host drives tab-completion and the
    /// worksheet from.
    pub fn bindings(&self) -> Vec<(String, Value)> {
        self.mobile.scope.all_bindings()
    }

    /// The largest single lexical binding's shallow byte estimate — the
    /// `` `largest-binding-bytes `` probe's reading, taken by reference: a
    /// probe that cloned the scope to size it would be its own cautionary
    /// tale.
    pub fn largest_binding_shallow_size(&self) -> usize {
        self.mobile.scope.largest_shallow_size()
    }

    /// Every bound name with its installed scheme, innermost binding wins —
    /// the scope half of [`Self::session_schemes`], surfaced on its own for
    /// the worksheet's type column.
    pub fn binding_schemes(&self) -> Vec<(String, Option<crate::typecheck::Scheme>)> {
        self.mobile.scope.binding_schemes()
    }

    /// The names of every installed handler entry — `within` arms and aliases
    /// alike — for tab completion.  The handler-stack counterpart of
    /// [`Self::builtin_names`].
    pub fn handler_names(&self) -> impl Iterator<Item = &str> {
        self.mobile
            .context
            .handlers
            .entries()
            .map(|e| e.name.as_ref())
    }

    /// Run `f` under a fresh innermost lexical scope, popped on return — the
    /// isolation `use` and the REPL plugin loader share, so a loaded file's
    /// top-level helpers die with the frame instead of leaking into the
    /// caller's scope.  [`crate::builtins::modules::evaluate_source`] owns the
    /// cycle and depth guards and the source registration; this owns only the
    /// scope frame.
    pub fn in_fresh_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.mobile.scope.clone();
        let r = f(self);
        self.mobile.scope = saved;
        r
    }

    /// The next run's check seed, read off the live session: every scope
    /// binding with its installed scheme, plus the alias arms' schemes off the
    /// persistent handler frames.
    pub fn session_schemes(&self) -> crate::typecheck::SessionSchemes {
        crate::typecheck::SessionSchemes {
            bindings: self.mobile.scope.binding_schemes(),
            aliases: self.mobile.context.handlers.alias_schemes(),
            builtins: self.session.builtins.clone(),
        }
    }

    /// Remove the alias for `name`, returning whether anything was removed.
    pub fn remove_alias(&mut self, name: &str) -> bool {
        self.mobile.context.handlers.remove_alias(name).is_some()
    }

    /// True if an alias frame is installed for `name`.  A scoped `within`
    /// frame is not an alias even when it has the same one-entry shape.
    pub fn has_alias(&self, name: &str) -> bool {
        self.mobile
            .context
            .handlers
            .iter()
            .any(|f| f.is_alias_for(name))
    }

    /// The winning handler for `name` — a run frame (with its depth) or a
    /// base frame.  A named run-frame entry at any depth outranks every
    /// catch-all, and a base frame outranks a catch-all too.
    pub fn lookup_handler(&self, name: &str) -> Option<crate::types::HandlerLookup> {
        self.mobile.context.handlers.lookup(name)
    }
}
