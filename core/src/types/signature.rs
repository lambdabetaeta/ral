//! Σ: the natives and the frozen prelude — constants of a running shell,
//! never part of any environment.  [`lookup`] is the one resolution rule: ρ,
//! then Σ's prelude, then Σ's natives.

use super::env::{Binding, Env};
use crate::types::Value;
use rustc_hash::FxBuildHasher;
use std::collections::HashMap;
use std::sync::Arc;

/// Every key here is a program identifier, never attacker-controlled input,
/// so both tiers use a fast non-cryptographic hasher.
pub(crate) type NativeMap = HashMap<String, Value, FxBuildHasher>;
pub(crate) type PreludeMap = HashMap<String, Binding, FxBuildHasher>;

/// Σ: language-given constants (seeded once at boot,
/// [`Signature::install_natives`]) and the baked prelude's bindings (seeded
/// once per shell, [`BakedPrelude::seat`](crate::boot::BakedPrelude::seat)).  A `Signature` is one per
/// shell, `Arc`-shared into every fork; the prelude inside it is
/// process-wide, one map baked once and `Arc`-shared into every shell's
/// `Signature` in turn, never copied.
#[derive(Debug, Clone, Default)]
pub struct Signature {
    natives: NativeMap,
    prelude: Arc<PreludeMap>,
}

impl Signature {
    /// Σ alone: prelude first, since a prelude name shadows a native of the
    /// same spelling.
    pub(crate) fn get(&self, name: &str) -> Option<&Value> {
        self.prelude
            .get(name)
            .map(|b| &b.value)
            .or_else(|| self.natives.get(name))
    }

    /// The prelude [`Binding`] for `name`; it carries the checker's harvested
    /// scheme, so a prelude function's type needs no separate registry.
    pub(crate) fn prelude_binding(&self, name: &str) -> Option<&Binding> {
        self.prelude.get(name)
    }

    pub(crate) fn prelude_names(&self) -> impl Iterator<Item = &str> {
        self.prelude.keys().map(String::as_str)
    }

    /// Seed the native tier — a value manifest row's `Value`, or a
    /// language-given constant.  Called only at boot, beside builtin-table
    /// installation.
    pub(crate) fn install_natives(&mut self, entries: impl IntoIterator<Item = (String, Value)>) {
        self.natives.extend(entries);
    }

    /// Seat the baked prelude tier — once per shell, right after the bake, an
    /// `Arc` clone of the one process-wide map so every shell shares it.
    pub(crate) fn install_prelude(&mut self, prelude: Arc<PreludeMap>) {
        self.prelude = prelude;
    }
}

/// The one resolution rule: ρ, then Σ's prelude, then Σ's natives.
/// `evaluator::val::form` and `command_call::resolve` are its callers.
pub(crate) fn lookup<'a>(name: &str, env: &'a Env, sig: &'a Signature) -> Option<&'a Value> {
    env.get(name).or_else(|| sig.get(name))
}
