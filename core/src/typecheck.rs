//! Hindley-Milner inference over the CBPV IR: value types on `Val`,
//! computation types on `Comp`, polymorphism by let-generalisation.
//!
//! Seeding and entry points live here; the rules generate constraints, then read back a closed solution.

pub mod builtins;
pub mod contract;
mod env;
mod error;
mod explain;
mod generalize;
mod generate;
mod readback;
#[cfg(test)]
mod session_tests;
mod site;
mod solve;
mod unify;

pub use self::builtins::{Decl, Manifest, builtin_type_hint};
pub use self::contract::{Form, Table};
pub use self::error::{
    CycleVia, KindFound, Reason, Standing, TypeError, TypeErrorKind, UnderAppliedBuiltin,
};
pub(crate) use self::explain::{
    NO_STATUS_REGISTER, hyphen_boundary, hyphen_boundary_hint, spread_into_application,
};
pub(crate) use self::unify::Unifier;

#[cfg(any(test, feature = "test-util"))]
pub(crate) use self::generalize::has_result_only_var;
pub(crate) use self::generalize::{instantiate, reseed_weak};
use self::generate::{Generator, call::Arm};
use self::readback::Readback;
use self::solve::Solver;
use crate::ir::{Comp, Phrase, Toplevel, Val};
use crate::source::Spanned;
use crate::ty::{Closed, CompTy, Ty, TyNode, UnitId};
use std::sync::Arc;

/// What a form holds its programs' own return value to: the declared
/// [`Table`] whose closed keyset the returned row is checked against.
///
/// An rc file's top-level keys, a plugin manifest's fields, a capability
/// profile's dimensions.  `&'static`, so a host crate with a table of its own
/// passes it here.
pub type ReturnContract = &'static Table;

/// The seed of a run's check, read off the live session by
/// `Shell::session_schemes`.
///
/// A binding with no scheme came from `Shell::set_var` — a host seed var, an
/// rc's `env:`/`prompt:` key — and is bound at one fresh monomorphic type
/// variable: generalising it to `∀a. a` would let it stand in for any type at
/// all, unsoundly.
#[derive(Debug, Clone)]
pub struct SessionSchemes {
    pub(crate) bindings: Vec<(String, Option<Closed>)>,
    pub(crate) aliases: Vec<(String, Closed)>,
    pub(crate) builtins: Manifest,
}

impl SessionSchemes {
    /// No bindings, no aliases: only Σ.
    pub fn new(builtins: Manifest) -> Self {
        Self {
            bindings: Vec::new(),
            aliases: Vec::new(),
            builtins,
        }
    }

    /// Seed from the baked prelude's own schemes, against a host surface's
    /// manifest, for callers with no live shell: `--check`, batch, tests.
    pub fn from_prelude(prelude: &Toplevel, builtins: Manifest) -> Self {
        Self {
            bindings: prelude
                .exported_schemes()
                .into_iter()
                .map(|(name, scheme)| (name, Some(scheme)))
                .collect(),
            ..Self::new(builtins)
        }
    }
}

fn seeded_generator(schemes: SessionSchemes) -> Generator {
    let mut solver = Solver::new();
    let (env, residuals) = solver.seed(schemes);
    Generator::new(solver, env, residuals)
}

/// The first of a refused unit's errors.
fn first_error(errors: Vec<TypeError>) -> Box<TypeError> {
    Box::new(
        errors
            .into_iter()
            .next()
            .expect("a refused unit has an error"),
    )
}

/// Exit the inference region before generalising, and export only after closure.
fn close_thunk_scheme(generator: Generator, cty: CompTy) -> Result<Closed, Box<TypeError>> {
    let scheme = generator.solver.generalise(&Ty::thunk(cty));
    let solution = generator.solver.close().map_err(first_error)?;
    Ok(solution.settle(Arc::new(scheme)))
}

/// Type-check `phrases`, seeding from the live session.
///
/// Infer each phrase in order, building its checked IR beside its type and
/// extending `TyEnv` at each `Define`. After ascription succeeds, freeze sites,
/// settle `Define` schemes and build readmissions under one fresh unit. Only closed
/// schemes leave — a run's unifier dies with the run and its variable ids
/// restart at zero, so an open scheme from run *N* would alias run
/// *N+1*'s fresh variables.
///
/// A [`ReturnContract`] additionally ascribes the declared table to the
/// phrases' own return value, once inference has finished (`contract::ascribe`).  The
/// *inferred* type is what is checked, so a key misspelled inside a spread is
/// caught with one written out.  A return typed at a variable stays on the
/// caller's own runtime check, which dispatches off the same table; a factory
/// is held to a block of the options, and what it makes is ascribed as a
/// direct return is.
///
/// # Errors
/// Every diagnostic inference collected, whenever that list is non-empty.
/// Erroneous units are refused whole and their inferred IR is dropped without freezing.
pub fn typecheck(
    phrases: &[Spanned<Phrase<()>>],
    schemes: SessionSchemes,
    contract: Option<ReturnContract>,
) -> Result<Toplevel, Vec<TypeError>> {
    let mut generator = seeded_generator(schemes);
    let (pending, tail) = generator.phrases(phrases);
    let solution = generator.solver.close()?;
    if let Some(table) = contract {
        contract::ascribe(&solution, phrases.last(), tail, table)?;
    }
    Ok(
        Readback::new(&solution, UnitId::fresh(), generator.synth_counter)
            .toplevel(&pending, &generator.residual_uses),
    )
}

/// Type-check the prelude IR against `manifest`, returning the annotated
/// [`Toplevel`].
///
/// Callers (`boot::bake_prelude`, from each host's build script)
/// serialise the annotated prelude, so running it installs each binding's
/// scheme beside its value, and [`SessionSchemes::from_prelude`] reads the
/// same schemes back off it.  A prelude binding named after a native seeds and
/// shadows like any other.
///
/// # Panics
/// If the prelude fails to type-check, reporting the errors.
pub fn bake_prelude(phrases: &[Spanned<Phrase<()>>], manifest: &Manifest) -> Toplevel {
    match typecheck(phrases, SessionSchemes::new(manifest.clone()), None) {
        Ok(annotated) => annotated,
        Err(errs) => {
            let msgs: Vec<String> = errs.iter().map(ToString::to_string).collect();
            panic!("prelude type errors:\n{}", msgs.join("\n"));
        }
    }
}

/// The scheme for a handler arm, computed by `HandlerEntry::vet` at install
/// and persisted only on alias frames, which outlive their run: the arm rule
/// ([`Generator::arm`]) over the closure's comp, closed against its
/// own unifier so a later run's check can be seeded with it.
///
/// # Errors
/// The arm takes other than the argv, or produces other than its head.
pub(crate) fn alias_arm_scheme(
    head: &str,
    arm: &Arc<Comp>,
    schemes: SessionSchemes,
) -> Result<Closed, Box<TypeError>> {
    let mut generator = seeded_generator(schemes);
    let (cty, _) =
        generator.region(|this| this.arm(&Val::thunk(Arc::clone(arm)), Arm::Named(head), None));
    close_thunk_scheme(generator, cty)
}

/// The computed catch-all's vet — `within [handler: $k]` — by the arm rule,
/// closed against its own unifier; no scheme persists, a catch-all frame not
/// outliving its run.
///
/// # Errors
/// The catch-all takes other than the name and the argv, or is no command.
pub(crate) fn catch_all_stands_in(
    arm: &Arc<Comp>,
    schemes: SessionSchemes,
) -> Result<(), Box<TypeError>> {
    let mut generator = seeded_generator(schemes);
    generator.region(|this| this.arm(&Val::thunk(Arc::clone(arm)), Arm::CatchAll, None));
    generator.solver.close().map(drop).map_err(first_error)
}

/// The scheme for a value binding (`Shell::bind_value`, `Shell::register_hook`):
/// the closure's comp inferred as written, closed against its own unifier.
pub(crate) fn binding_value_scheme(
    body: &Comp,
    schemes: SessionSchemes,
) -> Result<Closed, Box<TypeError>> {
    let mut generator = seeded_generator(schemes);
    let (cty, _) = generator.region(|this| this.scope(|this| this.comp(body)));
    close_thunk_scheme(generator, cty)
}

/// Whether a block of `scheme` takes `n` arguments and then produces: its
/// type is met, as an arm's is, so a type still a variable there fits.
pub(crate) fn takes(scheme: &Closed, n: usize) -> bool {
    let mut u = Unifier::new();
    let scheme = reseed_weak(&mut u, scheme);
    let instance = instantiate(&mut u, &scheme);
    let TyNode::Thunk(body) = instance.node() else {
        return false;
    };
    let params: Vec<Ty> = (0..n).map(|_| u.fresh_ty()).collect();
    let shape = CompTy::arrows(params, CompTy::ret(u.fresh_grade(), u.fresh_ty()));
    u.unify_comp_ty(&shape, body).is_ok()
}
