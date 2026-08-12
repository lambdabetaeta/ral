//! Builtin command bindings: command names implemented by host Rust code.
//!
//! The per-shell [`BuiltinTable`] is the boot manifest: it seeds the base env
//! scope (fixed-arity entries, as `Value::Native`) and the base handler
//! frames (open-argv entries) at construction, and backs `help` /
//! `explain`.  Dispatch never consults it — resolution is env → handlers →
//! external — and it admits no names: a user handler installs under any.
//!
//! [`BuiltinEntry::new`] is the sole constructor: `BuiltinBody` has no
//! bodiless variant, so no entry is expressible without a live body.

use super::flow::Settled;
use super::value::Value;
use crate::typecheck::builtins::BuiltinTypeRule;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, OnceLock};

/// Runtime closure backing a captured builtin body.
pub type CapturedBuiltinFn = Arc<
    dyn Fn(&[Value], &crate::types::Mooring, &mut crate::types::Shell) -> Settled<Value>
        + Send
        + Sync,
>;

/// Host implementation of a builtin command binding.
///
/// The [`Mooring`](crate::types::Mooring) is borrowed, not owned: the run's
/// fixed frame stays disjoint from the `&mut Shell` a body mutates.
#[derive(Clone)]
pub enum BuiltinBody {
    Static(fn(&[Value], &crate::types::Mooring, &mut crate::types::Shell) -> Settled<Value>),
    Captured(CapturedBuiltinFn),
}

impl fmt::Debug for BuiltinBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(_) => f.write_str("BuiltinBody::Static(<fn>)"),
            Self::Captured(_) => f.write_str("BuiltinBody::Captured(<closure>)"),
        }
    }
}

/// A builtin command binding; `doc` is the line `help` and `explain` print.
pub struct BuiltinEntry {
    pub name: Cow<'static, str>,
    pub type_rule: BuiltinTypeRule,
    pub doc: &'static str,
    body: BuiltinBody,
    /// [`Self::fixed_arity`]'s cache: a `Scheme` rule needs a fresh
    /// [`crate::typecheck::Unifier`] to derive its curry depth, so this
    /// spares every application step of a native that re-derivation.
    arity_cache: OnceLock<Option<usize>>,
}

impl BuiltinEntry {
    /// Build an entry with an empty arity cache — the one constructor.
    pub const fn new(
        name: Cow<'static, str>,
        type_rule: BuiltinTypeRule,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        Self {
            name,
            type_rule,
            doc,
            body,
            arity_cache: OnceLock::new(),
        }
    }

    /// Value-arg count a `$name` reference curries to; `None` when the argv
    /// is not fixed.  Structural, read off the type rule
    /// ([`BuiltinTypeRule::fixed_arity`]) once and cached: application calls
    /// this every apply step, and a `Scheme` rule's derivation is not free.
    pub fn fixed_arity(&self) -> Option<usize> {
        *self
            .arity_cache
            .get_or_init(|| self.type_rule.fixed_arity())
    }

    /// Invoke the body — reachable only with a proof that a
    /// [`crate::evaluator::audit::frame_call`] is already open around it.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub(crate) fn call_body(
        &self,
        _frame: &crate::evaluator::audit::Frame,
        args: &[Value],
        mooring: &crate::types::Mooring,
        shell: &mut crate::types::Shell,
    ) -> Settled<Value> {
        match &self.body {
            BuiltinBody::Static(f) => f(args, mooring, shell),
            BuiltinBody::Captured(f) => f(args, mooring, shell),
        }
    }
}

impl Clone for BuiltinEntry {
    /// Carries an already-computed arity cache forward.
    fn clone(&self) -> Self {
        let arity_cache = OnceLock::new();
        if let Some(a) = self.arity_cache.get() {
            let _ = arity_cache.set(*a);
        }
        Self {
            name: self.name.clone(),
            type_rule: self.type_rule,
            doc: self.doc,
            body: self.body.clone(),
            arity_cache,
        }
    }
}

impl fmt::Debug for BuiltinEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltinEntry")
            .field("name", &self.name)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

/// Per-shell builtin command bindings.  Names are disjoint across installed
/// sets — a collision panics at install — so lookup order never shadows.
#[derive(Debug, Clone, Default)]
pub struct BuiltinTable {
    sets: imbl::Vector<Arc<[BuiltinEntry]>>,
}

impl BuiltinTable {
    /// Install a group of builtin entries for this shell.  `true` if a new
    /// set was actually added, so a no-op reinstall does not seed the base
    /// scope and frames twice.
    ///
    /// # Panics
    /// If a name collides with an installed builtin or repeats in `entries`.
    pub fn install_static(&mut self, entries: &'static [BuiltinEntry]) -> bool {
        self.install_arc(Arc::from(entries))
    }

    /// Install runtime-owned builtin entries for this shell.
    ///
    /// Idempotent: a set already here — by `Arc` identity, or by carrying the
    /// same names — reinstalls as a no-op, reported by the `false` return.
    ///
    /// # Panics
    /// If a name collides with a *different* installed set — host crates must
    /// own disjoint surfaces — or repeats in `entries`.
    pub fn install_arc(&mut self, entries: Arc<[BuiltinEntry]>) -> bool {
        if self
            .sets
            .iter()
            .any(|set| Arc::ptr_eq(set, &entries) || same_builtin_names(set, &entries))
        {
            return false;
        }
        if let Err(e) = check_builtin_collisions(&entries, &self.sets) {
            panic!("builtin installation failed: {e}");
        }
        self.sets.push_back(entries);
        true
    }

    /// Look up a builtin by name.
    pub fn get(&self, name: &str) -> Option<BuiltinEntry> {
        self.sets
            .iter()
            .rev()
            .flat_map(|set| set.iter())
            .find(|entry| entry.name == name)
            .cloned()
    }

    /// Names of installed builtins, newest installed set first.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sets
            .iter()
            .rev()
            .flat_map(|set| set.iter().map(|entry| entry.name.as_ref()))
    }

    /// The base native scope this manifest implies: every fixed-arity entry's
    /// [`Value::Native`] plus the language constants — what a wire-hydrated
    /// `Env` carries, since natives cross the wire by name only and a
    /// receiver rebuilds them from its own manifest.
    pub(crate) fn natives_arc(&self) -> Arc<std::collections::HashMap<String, Value>> {
        let mut map = std::collections::HashMap::new();
        for set in &self.sets {
            for entry in set.iter() {
                if let Some(value) = native_value(entry) {
                    map.insert(entry.name.clone().into_owned(), value);
                }
            }
        }
        map.extend(language_constants());
        Arc::new(map)
    }
}

/// `true` and `false` — language-given names in every base scope, live and
/// hydrated alike, though they are not manifest entries.
pub(crate) fn language_constants() -> [(String, Value); 2] {
    [
        ("true".to_string(), Value::Bool(true)),
        ("false".to_string(), Value::Bool(false)),
    ]
}

/// `entry`'s `Value::Native`, or `None` for an open-argv entry, which
/// seeds a base handler frame instead.  Shared by boot and wire hydration,
/// so the two never classify an entry differently.
pub(crate) fn native_value(entry: &BuiltinEntry) -> Option<Value> {
    entry.fixed_arity().map(|_| {
        let entry = Arc::new(entry.clone());
        Value::Native {
            entry,
            applied: Vec::new(),
        }
    })
}

fn same_builtin_names(a: &[BuiltinEntry], b: &[BuiltinEntry]) -> bool {
    a.len() == b.len()
        && a.iter()
            .map(|entry| entry.name.as_ref())
            .all(|name| b.iter().any(|entry| entry.name == name))
}

fn check_builtin_collisions(
    new_entries: &[BuiltinEntry],
    installed: &imbl::Vector<Arc<[BuiltinEntry]>>,
) -> Result<(), String> {
    let mut local = HashSet::new();
    for entry in new_entries {
        let name = entry.name.as_ref();
        if !local.insert(name) {
            return Err(format!(
                "builtin `{name}` is installed twice in one builtin set"
            ));
        }
        if installed
            .iter()
            .flat_map(|set| set.iter())
            .any(|existing| existing.name == name)
        {
            return Err(format!(
                "builtin `{name}` conflicts with an installed builtin"
            ));
        }
    }
    Ok(())
}
