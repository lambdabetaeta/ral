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
//! [`BuiltinEntry::new`], [`BuiltinEntry::boundary`] and
//! [`BuiltinEntry::base_frame`] are the only constructors — the value half's
//! two kinds of row, and the argv half's: `BuiltinBody` has no bodiless
//! variant, so no entry is expressible without a live body.

use super::flow::Settled;
use super::site::Site;
use super::value::Value;
use crate::typecheck::builtins::{BuiltinDiagnostic, BuiltinTypeRule, scheme_curry_depth};
use crate::typecheck::{CompTy, Scheme, Ty, Unifier};
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, OnceLock};

/// Runtime closure backing a captured builtin body.
pub(crate) type CapturedBuiltinFn = Arc<
    dyn Fn(&[Value], &crate::types::Mooring, &mut crate::types::Shell) -> Settled<Value>
        + Send
        + Sync,
>;

/// Host implementation of a builtin command binding.
///
/// The [`Mooring`](crate::types::Mooring) is borrowed, not owned: the run's
/// fixed frame stays disjoint from the `&mut Shell` a body mutates.
/// A door: handed the [`Site`] the checker solved at its call, which it
/// admits the value it lets in against.
pub type BoundaryFn =
    fn(&[Value], &Arc<Site>, &crate::types::Mooring, &mut crate::types::Shell) -> Settled<Value>;

#[derive(Clone)]
pub enum BuiltinBody {
    Static(fn(&[Value], &crate::types::Mooring, &mut crate::types::Shell) -> Settled<Value>),
    Captured(CapturedBuiltinFn),
    Boundary(BoundaryFn),
}

impl fmt::Debug for BuiltinBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(_) => f.write_str("BuiltinBody::Static(<fn>)"),
            Self::Captured(_) => f.write_str("BuiltinBody::Captured(<closure>)"),
            Self::Boundary(_) => f.write_str("BuiltinBody::Boundary(<fn>)"),
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

/// What a row does with stdout, declared per row because a body's writing is
/// not in its signature.  A `Writes` row answers `Unit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Returns,
    Writes,
}

/// A builtin command binding; `doc` is the line `help` and `explain` print.
pub struct BuiltinEntry {
    pub name: Cow<'static, str>,
    pub convention: Convention,
    pub(crate) type_rule: BuiltinTypeRule,
    pub doc: &'static str,
    pub output: Output,
    /// Extra non-typing behaviour the checker's application path reads: which
    /// diagnostic an over-application or a literal misuse earns.  `None` for
    /// the overwhelming majority of rows.
    pub(crate) diagnostic: BuiltinDiagnostic,
    body: BuiltinBody,
    /// [`Self::fixed_arity`]'s cache: a `Scheme` rule needs a fresh
    /// [`Unifier`] to derive its curry depth, so this spares every
    /// application step of a native that re-derivation.
    arity_cache: OnceLock<usize>,
    /// [`Self::settles_at_unit`]'s cache, derived the same way and for the
    /// same reason.
    unit_cache: OnceLock<bool>,
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
            output: Output::Returns,
            diagnostic: BuiltinDiagnostic::None,
            body,
            arity_cache: OnceLock::new(),
            unit_cache: OnceLock::new(),
        }
    }

    /// A value row that is a boundary: its result enters typed code through
    /// `body`, which admits it against the checker's [`Site`].
    pub const fn boundary(
        name: Cow<'static, str>,
        type_rule: BuiltinTypeRule,
        doc: &'static str,
        body: BoundaryFn,
    ) -> Self {
        Self::new(name, type_rule, doc, BuiltinBody::Boundary(body))
    }

    /// Whether the row is a boundary, so a call of it carries a [`Site`].
    pub fn is_boundary(&self) -> bool {
        matches!(self.body, BuiltinBody::Boundary(_))
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
            type_rule: argv,
            doc,
            output: Output::Returns,
            diagnostic: BuiltinDiagnostic::None,
            body,
            arity_cache: OnceLock::new(),
            unit_cache: OnceLock::new(),
        }
    }

    /// Declare what the row does with stdout — a builder, so the common case
    /// (`Returns`) names nothing.
    pub const fn with_output(mut self, output: Output) -> Self {
        self.output = output;
        self
    }

    /// Attach a diagnostic facet to an otherwise-built entry — a builder
    /// rather than a `new`/`base_frame` parameter, so the common case names
    /// none.
    pub(crate) const fn with_diagnostic(mut self, diagnostic: BuiltinDiagnostic) -> Self {
        self.diagnostic = diagnostic;
        self
    }

    /// The curry depth of this row's type — for a value row, the argument
    /// count a `$name` reference saturates at, and the checker's own arity
    /// diagnostics for a call at command position, not only the evaluator's
    /// arity gate.  Structural, read off the type rule's curry spine
    /// ([`scheme_curry_depth`]) once and cached: application calls this every
    /// apply step, and instantiating a scheme fresh is not free.
    pub(crate) fn fixed_arity(&self) -> usize {
        *self
            .arity_cache
            .get_or_init(|| scheme_curry_depth(self.type_rule))
    }

    /// Whether the row's declared scheme settles at `Unit`, read off the type
    /// rule's curry spine and cached like [`Self::fixed_arity`].
    fn settles_at_unit(&self) -> bool {
        *self.unit_cache.get_or_init(|| {
            fn settled(ct: &CompTy) -> Option<&Ty> {
                match ct {
                    CompTy::Fun(_, body) => settled(body),
                    CompTy::Return(_, ty) => Some(ty),
                    CompTy::Var(_) => None,
                }
            }
            let scheme = (self.type_rule)(&mut Unifier::new());
            let Ty::Thunk(inner) = &scheme.ty else {
                return false;
            };
            matches!(settled(inner), Some(Ty::Unit))
        })
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
        site: Option<&Arc<Site>>,
        mooring: &crate::types::Mooring,
        shell: &mut crate::types::Shell,
    ) -> Settled<Value> {
        let value = match (&self.body, site) {
            (BuiltinBody::Static(f), _) => f(args, mooring, shell),
            (BuiltinBody::Captured(f), _) => f(args, mooring, shell),
            (BuiltinBody::Boundary(f), Some(site)) => f(args, site, mooring, shell),
            // `annotate` gives every boundary call a site; only a host calling
            // a door outside a checked program reaches this.
            (BuiltinBody::Boundary(_), None) => Err(crate::types::sig(format!(
                "{}: reached without a checked site",
                self.name
            ))),
        }?;
        // The declared scheme is the authority on what a row settles to, so a
        // body cannot put an inhabitant of another type under `F Unit` — which
        // a body and its scheme agreeing only by hand otherwise allows.
        // Asserted in debug; coerced to `Unit` in release regardless of what
        // the body answered, since the scheme, not the body, is authoritative.
        if self.settles_at_unit() {
            debug_assert!(
                matches!(value, Value::Unit),
                "{}: typed `F Unit` but answered a {}",
                self.name,
                value.type_name()
            );
            return Ok(Value::Unit);
        }
        Ok(value)
    }
}

impl Clone for BuiltinEntry {
    /// Carries an already-computed arity/unit cache forward.
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            convention: self.convention,
            type_rule: self.type_rule,
            doc: self.doc,
            output: self.output,
            diagnostic: self.diagnostic,
            body: self.body.clone(),
            arity_cache: self.arity_cache.clone(),
            unit_cache: self.unit_cache.clone(),
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
    pub(crate) fn install_static(&mut self, entries: &'static [BuiltinEntry]) -> bool {
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
    pub(crate) fn install_arc(&mut self, entries: Arc<[BuiltinEntry]>) -> bool {
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

    /// Every base-frame row — what the handler stack and the checker's handler
    /// bindings are both seeded from.
    pub(crate) fn base_frames(&self) -> impl Iterator<Item = &BuiltinEntry> {
        self.rows()
            .filter(|entry| entry.convention == Convention::Argv)
    }

    /// Names a `$name` reference reaches: the value rows.
    pub(crate) fn value_names(&self) -> impl Iterator<Item = &str> {
        self.rows()
            .filter(|entry| entry.convention == Convention::Value)
            .map(|entry| entry.name.as_ref())
    }

    /// Names of installed builtins, newest installed set first.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.rows().map(|entry| entry.name.as_ref())
    }
}

/// `true` and `false` — language-given names in every base scope, live and
/// hydrated alike, though they are not manifest entries.  The checker types
/// them `Bool`.
pub(crate) const LANGUAGE_CONSTANTS: [(&str, bool); 2] = [("true", true), ("false", false)];

pub(crate) fn language_constants() -> impl Iterator<Item = (String, Value)> {
    LANGUAGE_CONSTANTS
        .into_iter()
        .map(|(name, b)| (name.to_string(), Value::Bool(b)))
}

/// A value row's `Value::Native`, unapplied.  Shared by boot and wire
/// hydration, so the two never build one differently.
pub(crate) fn native_value(entry: &BuiltinEntry) -> Value {
    Value::Native {
        entry: Arc::new(entry.clone()),
        applied: Box::new([]),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{
        BOUNDARY_BUILTINS, CORE_BASE_FRAMES, CORE_BUILTINS, SERVICE_BUILTIN, SURFACE_BUILTIN,
        WATCH_BUILTIN,
    };
    fn result(ct: &CompTy) -> Option<&Ty> {
        match ct {
            CompTy::Fun(_, body) => result(body),
            CompTy::Return(_, ty) => Some(ty),
            CompTy::Var(_) => None,
        }
    }

    /// The soundness perimeter: a value of a type the program did not decide
    /// enters typed code only through a boundary.  A row whose scheme
    /// quantifies a variable only its result mentions is such a door, so each
    /// must be a boundary or diverge.  The enumeration is not the whole
    /// perimeter: two casts lived in term rules rather than in Σ — `$ENV`,
    /// typed `Map String`, and a bound head that is not a block, refused as
    /// `HeadBoundToValue` — and `tests/reject` pins those.  The exarch and
    /// plugin tables are swept by their own crates through the same predicate
    /// (`test_access::has_result_only_var`).
    #[test]
    fn a_result_only_variable_is_a_boundary_or_divergence() {
        const DIVERGENT: [&str; 3] = ["fail", "exit", "quit"];
        let sets: [&[BuiltinEntry]; 6] = [
            CORE_BUILTINS,
            BOUNDARY_BUILTINS,
            CORE_BASE_FRAMES,
            WATCH_BUILTIN,
            SERVICE_BUILTIN,
            SURFACE_BUILTIN,
        ];
        #[cfg(unix)]
        let detach = crate::builtins::DETACH_BUILTIN;
        #[cfg(not(unix))]
        let detach: &[BuiltinEntry] = &[];
        for entry in sets.into_iter().flatten().chain(detach) {
            let name = entry.name.as_ref();
            let scheme = (entry.type_rule)(&mut Unifier::new());
            let casts = crate::typecheck::has_result_only_var(&scheme);
            let allowed = entry.is_boundary() || DIVERGENT.contains(&name);
            assert!(
                !casts || allowed,
                "{name}: a result only its own call determines, and it is no boundary"
            );
            assert!(
                casts || !entry.is_boundary() || name == "_ed-state",
                "{name}: a boundary whose result is decided by its arguments is no door"
            );
        }
    }

    /// Every boundary says so in its scheme: marked boundaries are exactly the
    /// core rows whose result the program did not decide.
    #[test]
    fn the_core_boundaries_are_the_decoders_and_use() {
        let names: Vec<&str> = BOUNDARY_BUILTINS.iter().map(|e| e.name.as_ref()).collect();
        assert_eq!(names, ["from-json", "from-jsonl", "from-json-at", "use"]);
        assert!(CORE_BUILTINS.iter().all(|e| !e.is_boundary()));
    }

    /// `Writes` is declared, not derived; this is what keeps the declaration
    /// honest: a row that writes answers `Unit`.
    #[test]
    fn a_writing_row_answers_unit() {
        let sets: [&[BuiltinEntry]; 6] = [
            CORE_BUILTINS,
            BOUNDARY_BUILTINS,
            CORE_BASE_FRAMES,
            WATCH_BUILTIN,
            SERVICE_BUILTIN,
            SURFACE_BUILTIN,
        ];
        #[cfg(unix)]
        let detach = crate::builtins::DETACH_BUILTIN;
        #[cfg(not(unix))]
        let detach: &[BuiltinEntry] = &[];
        for entry in sets.into_iter().flatten().chain(detach) {
            let scheme = (entry.type_rule)(&mut Unifier::new());
            let Ty::Thunk(inner) = &scheme.ty else {
                panic!("{}: a row's scheme is a thunk", entry.name);
            };
            if entry.output == Output::Writes {
                assert!(
                    matches!(result(inner), Some(Ty::Unit)),
                    "{}: a writing row's result is `F Unit`",
                    entry.name
                );
            }
        }
    }
}
