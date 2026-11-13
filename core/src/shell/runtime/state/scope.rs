//! The persistent-handler lifecycle and the lexical scope doors.
//!
//! ral evaluation never unwinds for control flow, so each guard here is an
//! inline save-modify-restore around the body rather than an RAII type.

use super::Shell;
use crate::ir::{Binding, Env, ValClos};
use crate::shell::runtime::{FrameHandle, HandlerEntry, Settled};
use crate::ty::Closed;

impl Shell {
    /// Install `thunk` as the persistent handler for `name`, replacing any existing one.
    /// Sibling of
    /// [`WithinScope::enter`](crate::shell::evaluator::scope::WithinScope::enter):
    /// same handler stack, but a persistent frame is never popped at scope exit,
    /// only by [`Self::remove_handler`].
    /// [`HandlerEntry::vet`] checks the arm against its command before installation.
    ///
    /// # Errors
    /// `thunk` not an arm: a block of one parameter, the argv, producing what
    /// its head produces.
    pub fn install_handler(&mut self, name: String, thunk: ValClos) -> Settled<FrameHandle> {
        self.vet_handler(name, thunk)
            .map(|entry| self.commit_handler(entry))
    }

    /// The read-only half of [`Self::install_handler`]: the gate, nothing installed.
    ///
    /// # Errors
    /// As [`Self::install_handler`].
    pub fn vet_handler(&self, name: String, thunk: ValClos) -> Settled<HandlerEntry> {
        HandlerEntry::vet(name, thunk, self)
    }

    /// The committing half: replace any persistent handler of the same name.
    pub fn commit_handler(&mut self, entry: HandlerEntry) -> FrameHandle {
        self.context.handlers.remove_handler(&entry.name);
        self.context.handlers.push_handler(vec![entry])
    }

    /// Remove the handler frame [`Self::install_handler`] minted, reporting whether
    /// it was still there: a frame the user replaced has a new handle.
    pub fn remove_handler_frame(&mut self, handle: FrameHandle) -> bool {
        self.context.handlers.remove_by_handle(handle).is_some()
    }

    /// Bind `name` to `value` with its host-declared closed scheme.
    /// The host must supply the scheme that describes `value`.
    pub fn set_var(&mut self, name: String, value: ValClos, scheme: Closed) {
        self.env.bind(
            name.into(),
            Binding {
                value,
                scheme: Some(scheme),
            },
        );
    }

    /// Record a `Phrase::Define`'s binding on the lease ledger: starts a
    /// fresh non-baseline name's lease and renews an existing one — writing
    /// a name is itself interest in it — and, when the lease is armed and
    /// [`ValClos::shallow_size`] meets `large_binding_bytes`, queues a
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

    /// Look `name` up in the lexical scope chain, natives included.  The read
    /// dual of [`Self::set_var`].
    pub fn scope_lookup(&self, name: &str) -> Option<&ValClos> {
        crate::shell::runtime::lookup(name, &self.env, &self.sig)
    }

    /// Every lexical binding across the whole scope chain, innermost
    /// shadowing outermost.
    pub fn bindings(&self) -> Vec<(String, ValClos)> {
        self.env.all_bindings(&self.sig)
    }

    /// The largest single lexical binding's shallow byte estimate — the
    /// `` `largest-binding-bytes `` probe's reading, taken by reference: a
    /// probe that cloned the scope to size it would be its own cautionary
    /// tale.
    pub(crate) fn largest_binding_shallow_size(&self) -> usize {
        self.env.largest_shallow_size(&self.sig)
    }

    /// Every scoped and persistent handler name, for tab completion.
    /// The handler-stack counterpart of
    /// [`Self::builtin_names`].
    pub fn handler_names(&self) -> impl Iterator<Item = &str> {
        self.context.handlers.entries().map(|e| e.name.as_ref())
    }

    /// The next run's check seed, read off the live session: every scope
    /// binding with its installed scheme, plus installed handler names.
    pub fn session_schemes(&self) -> crate::typecheck::SessionSchemes {
        self.schemes_over(&self.env)
    }

    /// What a body over `env` is checked against: what `env` captured with its
    /// installed schemes, then Σ's prelude, beside the handlers in force.
    pub(crate) fn schemes_over(&self, env: &Env) -> crate::typecheck::SessionSchemes {
        crate::typecheck::SessionSchemes {
            bindings: env.binding_schemes(&self.sig),
            handled: self.context.handlers.handled_names(),
            builtins: self.session.runtime.builtins.manifest().clone(),
        }
    }

    /// Remove the persistent handler for `name`, reporting whether it was present.
    pub fn remove_handler(&mut self, name: &str) -> bool {
        self.context.handlers.remove_handler(name).is_some()
    }

    /// True if a persistent handler frame is installed for `name`.
    /// Scoped `within` frames do not count, even with the same shape.
    pub fn has_handler(&self, name: &str) -> bool {
        self.context.handlers.iter().any(|f| f.is_handler_for(name))
    }

    /// The winning handler for `name` — a run frame (with its depth) or a
    /// base frame.  A named run-frame entry at any depth outranks every
    /// catch-all, and a base frame outranks a catch-all too.
    pub(crate) fn lookup_handler(
        &self,
        name: &str,
    ) -> Option<crate::shell::runtime::HandlerLookup> {
        self.context.handlers.lookup(name)
    }
}
