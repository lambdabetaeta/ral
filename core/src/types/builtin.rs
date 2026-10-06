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

use super::flow::{Settled, name_failure};
use super::value::Value;
use super::{Mooring, Shell};
use crate::ty::Site;
use crate::typecheck::builtins::{
    BuiltinDiagnostic, BuiltinTypeRule, Convention, Decl, LANGUAGE_CONSTANTS, Manifest,
};
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// Runtime closure backing a captured builtin body.
pub(crate) type CapturedBuiltinFn =
    Arc<dyn Fn(&[Value], &Mooring, &mut Shell) -> Settled<Value> + Send + Sync>;

/// A door: handed the [`Site`] the checker solved at its call, which it
/// admits the value it lets in against.
pub type BoundaryFn = fn(&[Value], &Arc<Site>, &Mooring, &mut Shell) -> Settled<Value>;

/// Host implementation of a builtin command binding.
///
/// The [`Mooring`] is borrowed, not owned: the run's fixed frame stays
/// disjoint from the `&mut Shell` a body mutates.
#[derive(Clone)]
pub enum BuiltinBody {
    Static(fn(&[Value], &Mooring, &mut Shell) -> Settled<Value>),
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

/// Proof that a body runs inside [`BuiltinEntry::framed`]'s dynamic extent,
/// minted nowhere else, so [`BuiltinEntry::call_body`] cannot be reached
/// unframed.
pub(crate) struct Frame(());

/// A builtin command binding: its declaration, which the checker reads, and
/// the body only the runtime runs.
#[derive(Clone)]
pub struct BuiltinEntry {
    pub decl: Decl,
    body: BuiltinBody,
}

impl BuiltinEntry {
    const fn row(
        name: Cow<'static, str>,
        convention: Convention,
        type_rule: BuiltinTypeRule,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        let boundary = matches!(body, BuiltinBody::Boundary(_));
        Self {
            decl: Decl::new(name, convention, type_rule, doc, boundary),
            body,
        }
    }

    /// A value row: applied at the arity `type_rule` declares.
    pub const fn new(
        name: Cow<'static, str>,
        type_rule: BuiltinTypeRule,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        Self::row(name, Convention::Value, type_rule, doc, body)
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

    /// A base-frame row, typed by the scheme `argv` names.  A signature of
    /// argument templates is the value half's vocabulary and cannot be written
    /// here: this half has one argument, the argv, and one type for it.
    pub const fn base_frame(
        name: Cow<'static, str>,
        argv: BuiltinTypeRule,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        Self::row(name, Convention::Argv, argv, doc, body)
    }

    /// Attach a diagnostic facet to an otherwise-built entry.
    pub(crate) const fn with_diagnostic(mut self, diagnostic: BuiltinDiagnostic) -> Self {
        self.decl.diagnostic = diagnostic;
        self
    }

    /// Run `f` in this entry's call frame, which only names a failure: a
    /// builtin application is not an observation.
    pub(crate) fn framed(
        &self,
        shell: &mut Shell,
        f: impl FnOnce(&mut Shell, &Frame) -> Settled<Value>,
    ) -> Settled<Value> {
        let mut result = f(shell, &Frame(()));
        name_failure(&self.decl.name, &mut result);
        result
    }

    /// Invoke the body — reachable only with a proof that a [`Self::framed`]
    /// frame is already open around it.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub(crate) fn call_body(
        &self,
        _frame: &Frame,
        args: &[Value],
        site: Option<&Arc<Site>>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Value> {
        let value = match (&self.body, site) {
            (BuiltinBody::Static(f), _) => f(args, mooring, shell),
            (BuiltinBody::Captured(f), _) => f(args, mooring, shell),
            (BuiltinBody::Boundary(f), Some(site)) => f(args, site, mooring, shell),
            // `annotate` gives every boundary call a site; only a host calling
            // a door outside a checked program reaches this.
            (BuiltinBody::Boundary(_), None) => Err(crate::types::sig(format!(
                "{}: reached without a checked site",
                self.decl.name
            ))),
        }?;
        // The declared scheme, not the body, is authoritative: asserted in
        // debug, coerced to `Unit` in release.
        if self.decl.settles_at_unit() {
            debug_assert!(
                matches!(value, Value::Unit),
                "{}: typed `F Unit` but answered a {}",
                self.decl.name,
                value.type_name()
            );
            return Ok(Value::Unit);
        }
        Ok(value)
    }
}

impl fmt::Debug for BuiltinEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltinEntry")
            .field("name", &self.decl.name)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

/// One installed group of builtin rows, keeping the identity it was
/// installed under so a reinstall is recognised by pointer.
#[derive(Debug, Clone)]
pub(crate) enum Set {
    Static(&'static [BuiltinEntry]),
    Captured(Arc<[BuiltinEntry]>),
}

impl Deref for Set {
    type Target = [BuiltinEntry];

    fn deref(&self) -> &[BuiltinEntry] {
        match self {
            Self::Static(rows) => rows,
            Self::Captured(rows) => rows,
        }
    }
}

impl Set {
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Static(a), Self::Static(b)) => std::ptr::eq(*a, *b),
            (Self::Captured(a), Self::Captured(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Per-shell builtin command bindings.  Names are disjoint across installed
/// sets, so lookup order never shadows; the [`Manifest`] is each set's
/// declarations, projected once at install.
#[derive(Debug, Clone, Default)]
pub struct BuiltinTable {
    sets: imbl::Vector<Set>,
    manifest: Manifest,
}

impl BuiltinTable {
    /// Install a group of builtin entries for this shell.  `true` if a new
    /// set was actually added, so a no-op reinstall does not seed the base
    /// scope and frames twice.  The same set again, or one carrying the names
    /// of one already here, reinstalls as a no-op.
    ///
    /// # Panics
    /// If a name collides with a *different* installed set (host crates must
    /// own disjoint surfaces) or repeats within `set`.
    pub(crate) fn install(&mut self, set: Set) -> bool {
        if self.sets.iter().any(|have| have.same(&set)) {
            return false;
        }
        let decls: Arc<[Decl]> = set.iter().map(|entry| entry.decl.clone()).collect();
        if self.manifest.holds(&decls) {
            return false;
        }
        self.manifest.push(decls);
        self.sets.push_back(set);
        true
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn rows(&self) -> impl Iterator<Item = &BuiltinEntry> {
        self.sets.iter().rev().flat_map(|set| set.iter())
    }

    /// Any row by name, either half, with its body.
    pub fn get(&self, name: &str) -> Option<BuiltinEntry> {
        self.rows().find(|entry| entry.decl.name == name).cloned()
    }

    /// The *value* row for `name`, with its body.
    pub(crate) fn value(&self, name: &str) -> Option<BuiltinEntry> {
        self.rows()
            .find(|entry| entry.decl.name == name && entry.decl.convention == Convention::Value)
            .cloned()
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{
        BOUNDARY_BUILTINS, CORE_BASE_FRAMES, CORE_BUILTINS, SERVICE_BUILTIN, SURFACE_BUILTIN,
        WATCH_BUILTIN,
    };
    use crate::typecheck::Unifier;

    /// The soundness perimeter: a value of a type the program did not decide
    /// enters typed code only through a boundary.  A row whose scheme
    /// quantifies a variable only its result mentions is such a door, so each
    /// must be a boundary or diverge.  The enumeration is not the whole
    /// perimeter: two casts lived in term rules rather than in Σ — `env`,
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
            let name = entry.decl.name.as_ref();
            let scheme = (entry.decl.type_rule)(&mut Unifier::new());
            let casts = crate::typecheck::has_result_only_var(&scheme);
            let allowed = entry.decl.is_boundary() || DIVERGENT.contains(&name);
            assert!(
                !casts || allowed,
                "{name}: a result only its own call determines, and it is no boundary"
            );
            assert!(
                casts || !entry.decl.is_boundary() || name == "_ed-state",
                "{name}: a boundary whose result is decided by its arguments is no door"
            );
        }
    }

    /// Every boundary says so in its scheme: marked boundaries are exactly the
    /// core rows whose result the program did not decide.
    #[test]
    fn the_core_boundaries_are_the_decoders_and_use() {
        let names: Vec<&str> = BOUNDARY_BUILTINS
            .iter()
            .map(|e| e.decl.name.as_ref())
            .collect();
        assert_eq!(names, ["from-json", "from-jsonl", "from-json-at", "use"]);
        assert!(CORE_BUILTINS.iter().all(|e| !e.decl.is_boundary()));
    }
}
