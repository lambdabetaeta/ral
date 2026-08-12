//! Builtin command bindings: command names implemented by host Rust code.
//!
//! The per-shell [`BuiltinTable`] is the boot manifest, and it is authored as
//! two halves — ral's two argument conventions, one row apiece
//! ([`Convention`]).  It seeds the base env scope from the value half (as
//! `Value::Native`) and the base handler frames from the argv half at
//! construction, and backs `help` / `explain`.  Dispatch never consults it —
//! resolution is env → handlers → external — and it admits no names: a user
//! handler installs under any.
//!
//! [`BuiltinEntry::new`] and [`BuiltinEntry::base_frame`] are the only
//! constructors, one per half: `BuiltinBody` has no bodiless variant, so no
//! entry is expressible without a live body.

use super::flow::Settled;
use super::value::Value;
use crate::typecheck::builtins::BuiltinTypeRule;
use crate::typecheck::{Scheme, Unifier};
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

/// Which of ral's two argument conventions a manifest row uses.  The manifest
/// is authored as two, and what a name can do follows from which half it is in
/// rather than from its arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    /// Curried application at the arity the row's type declares, and
    /// first-class as `$name`: the row seeds the base env scope as a
    /// [`Value::Native`].
    Value,
    /// An argv, so the row seeds a base handler frame instead: intercepted,
    /// stacked, reached by `^name`, and never a value.  Typed `List String`,
    /// an argv's elements crossing rendered — though the body is handed the
    /// values themselves, and renders what it writes (`echo`) or vets what it
    /// launches (`detach`) as its own boundary demands.
    Argv,
}

/// A builtin command binding; `doc` is the line `help` and `explain` print.
pub struct BuiltinEntry {
    pub name: Cow<'static, str>,
    pub convention: Convention,
    pub type_rule: BuiltinTypeRule,
    pub doc: &'static str,
    body: BuiltinBody,
    /// [`Self::fixed_arity`]'s cache: a `Scheme` rule needs a fresh
    /// [`Unifier`] to derive its curry depth, so this spares every
    /// application step of a native that re-derivation.
    arity_cache: OnceLock<usize>,
}

impl BuiltinEntry {
    /// A value row: applied at the arity `type_rule` declares.
    pub const fn new(
        name: Cow<'static, str>,
        type_rule: BuiltinTypeRule,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        Self {
            name,
            convention: Convention::Value,
            type_rule,
            doc,
            body,
            arity_cache: OnceLock::new(),
        }
    }

    /// A base-frame row, typed by the scheme `argv` names.  A signature of
    /// argument templates is the value half's vocabulary and cannot be written
    /// here: this half has one argument, the argv, and one type for it.
    pub const fn base_frame(
        name: Cow<'static, str>,
        argv: fn(&mut Unifier) -> Scheme,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        Self {
            name,
            convention: Convention::Argv,
            type_rule: BuiltinTypeRule::Scheme(argv),
            doc,
            body,
            arity_cache: OnceLock::new(),
        }
    }

    /// The curry depth of this row's type — for a value row, the argument
    /// count a `$name` reference saturates at.  Structural, read off the type
    /// rule ([`BuiltinTypeRule::fixed_arity`]) once and cached: application
    /// calls this every apply step, and a `Scheme` rule's derivation is not
    /// free.
    pub fn fixed_arity(&self) -> usize {
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
            convention: self.convention,
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

    /// Every installed row, newest installed set first.
    fn rows(&self) -> impl Iterator<Item = &BuiltinEntry> {
        self.sets.iter().rev().flat_map(|set| set.iter())
    }

    /// Any manifest row by name, either half — what `help` and `explain`
    /// document.
    pub fn get(&self, name: &str) -> Option<BuiltinEntry> {
        self.rows().find(|entry| entry.name == name).cloned()
    }

    /// The *value* row for `name`: the half an application and a `$name`
    /// reference reach.  `None` for a base frame, which command position and
    /// `^name` reach through the handler stack instead.
    pub fn value(&self, name: &str) -> Option<BuiltinEntry> {
        self.rows()
            .find(|entry| entry.name == name && entry.convention == Convention::Value)
            .cloned()
    }

    /// Every value row — what the base native scope is built from.
    fn values(&self) -> impl Iterator<Item = &BuiltinEntry> {
        self.rows()
            .filter(|entry| entry.convention == Convention::Value)
    }

    /// Every base-frame row — what the handler stack and the checker's handler
    /// bindings are both seeded from.
    pub fn base_frames(&self) -> impl Iterator<Item = &BuiltinEntry> {
        self.rows()
            .filter(|entry| entry.convention == Convention::Argv)
    }

    /// Names of installed builtins, newest installed set first.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.rows().map(|entry| entry.name.as_ref())
    }

    /// The base native scope this manifest implies: every value row's
    /// [`Value::Native`] plus the language constants — what a wire-hydrated
    /// `Env` carries, since natives cross the wire by name only and a
    /// receiver rebuilds them from its own manifest.
    pub(crate) fn natives_arc(&self) -> Arc<std::collections::HashMap<String, Value>> {
        let mut map = std::collections::HashMap::new();
        for entry in self.values() {
            map.insert(entry.name.clone().into_owned(), native_value(entry));
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

/// A value row's `Value::Native`, unapplied.  Shared by boot and wire
/// hydration, so the two never build one differently.
pub(crate) fn native_value(entry: &BuiltinEntry) -> Value {
    Value::Native {
        entry: Arc::new(entry.clone()),
        applied: Vec::new(),
    }
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
