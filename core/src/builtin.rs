//! Builtin command bindings: command names implemented by host Rust code.
//!
//! The per-shell [`BuiltinTable`] is the boot manifest, and it is authored as
//! two halves — ral's two argument conventions, one row apiece
//! ([`Convention`]).  It seeds the base env scope from the value half (as
//! `ValClos::builtin`) and the base handler frames from the argv half at
//! construction, and backs `help` / `explain`.  Dispatch never consults it —
//! resolution is env → handlers → external — and it admits no names: a user
//! handler installs under any.
//!
//! [`BuiltinEntry::new`], [`BuiltinEntry::boundary`] and
//! [`BuiltinEntry::base_frame`] are the only constructors — the value half's
//! two kinds of row, and the argv half's. The convention carries a value
//! row's type rule or fixes an argv row at `[String] → Command`.
//! `BuiltinBody` has no bodiless variant, so every entry has a live body.

use crate::fact::{InvariantFailure, Reason, ResidualDefect};
use crate::ir::{Comp, CompKind, Name, synthetic};
use crate::shell::runtime::{Env, Settled};
use crate::shell::runtime::{Error, Mooring, Shell};
use crate::shell::runtime::{ValClos, View};
use crate::source::Spanned;
use crate::ty::Site;
use crate::typecheck::builtins::{
    BuiltinDiagnostic, BuiltinTypeRule, Convention, Decl, LANGUAGE_CONSTANTS, Manifest,
};
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, OnceLock};

/// Runtime closure backing a captured builtin body.
pub(crate) type CapturedBuiltinFn =
    Arc<dyn Fn(&[ValClos], &Mooring, &mut Shell) -> Settled<ValClos> + Send + Sync>;

/// A door: handed the [`Site`] the checker solved at its call, which it
/// admits the value it lets in against.
pub type BoundaryFn = fn(&[ValClos], &Arc<Site>, &Mooring, &mut Shell) -> Settled<ValClos>;

/// Host implementation of a builtin command binding.
///
/// The [`Mooring`] is borrowed, not owned: the run's fixed frame stays
/// disjoint from the `&mut Shell` a body mutates.
#[derive(Clone)]
pub enum BuiltinBody {
    Static(fn(&[ValClos], &Mooring, &mut Shell) -> Settled<ValClos>),
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
    eta: OnceLock<(Arc<Comp>, Name)>,
}

impl PartialEq for BuiltinEntry {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl BuiltinEntry {
    /// The λ-chain ending in δ, and the name δ reads its builtin from.
    pub(crate) fn eta(&self) -> &(Arc<Comp>, Name) {
        self.eta.get_or_init(|| {
            let builtin: Name = "%builtin".into();
            let args: Box<[Name]> = (0..self.decl.fixed_arity())
                .map(|i| synthetic("arg", i))
                .collect();
            let params = args.clone();
            let body = Arc::new(Spanned::synthetic(CompKind::Builtin {
                builtin: builtin.clone(),
                args,
            }));
            let eta = params.into_iter().rev().fold(body, |body, param| {
                Arc::new(Spanned::synthetic(CompKind::Lam { param, body }))
            });
            (eta, builtin)
        })
    }

    /// The η residual awaiting all but `supplied` arguments, over `env`.
    ///
    /// # Errors
    /// Unless supplied < arity and `env` binds exactly δ's builtin (this
    /// entry) and its first `supplied` arguments, which part does not hold.
    pub(crate) fn residual(
        self: &Arc<Self>,
        env: &Env,
        supplied: usize,
    ) -> Result<ValClos, ResidualDefect> {
        if supplied >= self.decl.fixed_arity() {
            return Err(ResidualDefect::Saturated);
        }
        if env.len() != supplied + 1 {
            return Err(ResidualDefect::EnvironmentSize(env.len().into()));
        }
        let (eta, builtin) = self.eta();
        let bound = env.get(builtin).ok_or(ResidualDefect::BuiltinUnbound)?;
        match bound.view() {
            View::Builtin(entry) if Arc::ptr_eq(self, entry) => {}
            _ => return Err(ResidualDefect::BuiltinForeign),
        }
        let mut comp = eta;
        for _ in 0..supplied {
            let CompKind::Lam { param, body } = &comp.item else {
                unreachable!("η has one λ per argument")
            };
            env.get(param)
                .ok_or_else(|| ResidualDefect::ArgumentUnbound(param.to_string()))?;
            comp = body;
        }
        Ok(ValClos::thunk(Arc::clone(comp), env))
    }

    const fn row(
        name: Cow<'static, str>,
        convention: Convention,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        let boundary = matches!(body, BuiltinBody::Boundary(_));
        Self {
            decl: Decl::new(name, convention, doc, boundary),
            body,
            eta: OnceLock::new(),
        }
    }

    /// A value row: applied at the arity `rule` declares.
    pub const fn new(
        name: Cow<'static, str>,
        rule: BuiltinTypeRule,
        doc: &'static str,
        body: BuiltinBody,
    ) -> Self {
        Self::row(name, Convention::Value(rule), doc, body)
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

    /// A base-frame row: its convention fixes `[String] → Command`.
    pub const fn base_frame(name: Cow<'static, str>, doc: &'static str, body: BuiltinBody) -> Self {
        Self::row(name, Convention::Argv, doc, body)
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
        span: Option<crate::source::Span>,
        shell: &mut Shell,
        f: impl FnOnce(&mut Shell, &Frame) -> Settled<ValClos>,
    ) -> Settled<ValClos> {
        shell.at_site(self.decl.name.clone(), span, |shell| {
            f(shell, &Frame(())).map_err(|b| b.locate(&self.decl.name, span, shell))
        })
    }

    /// Invoke the body — reachable only with a proof that a [`Self::framed`]
    /// frame is already open around it.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub(crate) fn call_body(
        &self,
        _frame: &Frame,
        args: &[ValClos],
        site: Option<&Arc<Site>>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<ValClos> {
        let value = match (&self.body, site) {
            (BuiltinBody::Static(f), _) => f(args, mooring, shell),
            (BuiltinBody::Captured(f), _) => f(args, mooring, shell),
            (BuiltinBody::Boundary(f), Some(site)) => f(args, site, mooring, shell),
            // Checking gives every boundary call a frozen site; only a host calling
            // a door outside a checked program reaches this.
            (BuiltinBody::Boundary(_), None) => Err(Error::new(Reason::Invariant(Box::new(
                InvariantFailure::BoundaryWithoutSite(self.decl.name.to_string()),
            )))
            .into()),
        }?;
        // The declared scheme, not the body, is authoritative: asserted in
        // debug, coerced to `Unit` in release.
        if self.decl.settles_at_unit() {
            debug_assert!(
                matches!(value.view(), View::Unit),
                "{}: typed `F Unit` but answered a {}",
                self.decl.name,
                value.type_name()
            );
            return Ok(ValClos::UNIT);
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
}

pub(crate) fn language_constants() -> impl Iterator<Item = (String, ValClos)> {
    LANGUAGE_CONSTANTS
        .into_iter()
        .map(|(name, b)| (name.to_string(), ValClos::bool(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::builtins::{
        BOUNDARY_BUILTINS, CORE_BASE_FRAMES, CORE_BUILTINS, SURFACE_BUILTIN, WATCH_BUILTIN,
    };

    #[test]
    fn residual_requires_exactly_this_builtin_and_the_supplied_prefix() {
        use crate::shell::runtime::Binding;

        let shell = crate::test_helper::core_shell();
        let entry = Arc::new(shell.lookup_builtin("has").unwrap());
        let (eta, builtin) = entry.eta();
        let mut env = Env::from_bindings(vec![(
            builtin.clone(),
            Binding {
                value: ValClos::builtin(Arc::clone(&entry)),
                scheme: None,
            },
        )])
        .unwrap();
        let arity = entry.decl.fixed_arity();
        let mut suffix = eta;
        for supplied in 0..arity {
            let value = entry.residual(&env, supplied).unwrap();
            let View::Thunk {
                comp,
                env: captured,
            } = value.view()
            else {
                panic!("an η suffix")
            };
            assert!(Arc::ptr_eq(comp, suffix));
            assert!(captured.ptr_eq(&env));
            let (held_entry, remaining) = value.residual().unwrap();
            assert!(Arc::ptr_eq(held_entry, &entry));
            assert_eq!(remaining, arity - supplied);
            assert_eq!(
                entry.residual(&Env::EMPTY, supplied).unwrap_err(),
                ResidualDefect::EnvironmentSize(0usize.into())
            );

            let mut wrong = env.clone();
            wrong.bind(
                builtin.clone(),
                Binding {
                    value: ValClos::builtin(Arc::new(entry.as_ref().clone())),
                    scheme: None,
                },
            );
            assert_eq!(
                entry.residual(&wrong, supplied).unwrap_err(),
                ResidualDefect::BuiltinForeign
            );
            let mut wrong = env.clone();
            wrong.bind(
                "extra".into(),
                Binding {
                    value: ValClos::UNIT,
                    scheme: None,
                },
            );
            assert_eq!(
                entry.residual(&wrong, supplied).unwrap_err(),
                ResidualDefect::EnvironmentSize((supplied + 2).into())
            );
            let mut wrong = env.clone();
            wrong.unset(builtin);
            wrong.bind(
                "extra".into(),
                Binding {
                    value: ValClos::UNIT,
                    scheme: None,
                },
            );
            assert_eq!(
                entry.residual(&wrong, supplied).unwrap_err(),
                ResidualDefect::BuiltinUnbound
            );
            if supplied > 0 {
                let mut wrong = env.clone();
                wrong.unset(&synthetic("arg", 0));
                wrong.bind(
                    "extra".into(),
                    Binding {
                        value: ValClos::UNIT,
                        scheme: None,
                    },
                );
                assert_eq!(
                    entry.residual(&wrong, supplied).unwrap_err(),
                    ResidualDefect::ArgumentUnbound(synthetic("arg", 0).to_string())
                );
            }
            let CompKind::Lam { param, body } = &suffix.item else {
                panic!("η has one lambda per argument")
            };
            env.bind(
                param.clone(),
                Binding {
                    value: ValClos::UNIT,
                    scheme: None,
                },
            );
            suffix = body;
        }
        for supplied in [arity, arity + 1] {
            assert_eq!(
                entry.residual(&env, supplied).unwrap_err(),
                ResidualDefect::Saturated
            );
        }
    }

    /// The soundness perimeter: a value of a type the program did not decide
    /// enters typed code only through a boundary.  A row whose scheme
    /// quantifies a variable only its result mentions is such a door, so each
    /// must be a boundary or diverge.  The enumeration is not the whole
    /// perimeter: a bound head that is not a block is refused as
    /// `HeadBoundToValue`, and `tests/reject` pins that rule. `env` is checked
    /// through the boundary table. The exarch and
    /// plugin tables are swept by their own crates through the same predicate
    /// (`test_access::has_result_only_var`).
    #[test]
    fn a_result_only_variable_is_a_boundary_or_divergence() {
        const DIVERGENT: [&str; 4] = ["fail", "raise", "exit", "quit"];
        let sets: [&[BuiltinEntry]; 5] = [
            CORE_BUILTINS,
            BOUNDARY_BUILTINS,
            CORE_BASE_FRAMES,
            WATCH_BUILTIN,
            SURFACE_BUILTIN,
        ];
        #[cfg(unix)]
        let detach = crate::shell::builtins::DETACH_BUILTIN;
        #[cfg(not(unix))]
        let detach: &[BuiltinEntry] = &[];
        for entry in sets.into_iter().flatten().chain(detach) {
            let name = entry.decl.name.as_ref();
            let scheme = entry.decl.scheme();
            let casts = crate::typecheck::has_result_only_var(scheme);
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

    #[test]
    fn a_boundary_without_a_site_remains_a_recoverable_invariant() {
        let mut shell = crate::test_helper::core_shell();
        let entry = &BOUNDARY_BUILTINS[0];
        let Err(crate::shell::runtime::Break::Error(error)) =
            entry.run(&[], &Mooring::adrift(), &mut shell)
        else {
            panic!("boundary refusal")
        };
        assert_eq!(
            error.reason,
            Reason::Invariant(Box::new(InvariantFailure::BoundaryWithoutSite(
                entry.decl.name.to_string()
            )))
        );
    }
}
