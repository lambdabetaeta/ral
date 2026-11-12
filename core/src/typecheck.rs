//! Hindley-Milner inference over the CBPV IR: value types on `Val`,
//! computation types on `Comp`, polymorphism by let-generalisation.
//!
//! Seeding and entry points live here; the rules generate constraints, then read back a closed solution.

pub mod builtins;
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
pub mod table;
mod unify;

pub use self::builtins::{Decl, Manifest, builtin_type_hint};
pub use self::error::{
    CycleVia, KindFound, OptionsContext, Reason, Standing, TypeError, TypeErrorKind,
    UnderAppliedBuiltin,
};
pub(crate) use self::explain::{hyphen_boundary, hyphen_boundary_hint, spread_into_application};
pub use self::table::{Form, Table};
pub(crate) use self::unify::Unifier;
pub use self::unify::WeakSource;

use self::builtins::Arm;
#[cfg(any(test, feature = "test-util"))]
pub(crate) use self::generalize::has_result_only_var;
pub(crate) use self::generalize::{instantiate, reseed_weak};
use self::generate::Generator;
use self::readback::Readback;
use self::solve::{Solution, Solver, sort_errors};
use crate::ir::{Comp, Phrase, Toplevel, Val};
use crate::source::Spanned;
use crate::ty::{Closed, CompTy, Ty, TyNode, UnitId};
use std::sync::Arc;

/// The seed of a run's check, read off the live session by
/// `Shell::session_schemes`.
///
/// Every session binding has a scheme. Captured block locals can still lack
/// one when a hook's closure is re-inferred; those receive fresh monomorphic
/// variables rather than the unsound scheme `∀a. a`.
#[derive(Debug, Clone)]
pub struct SessionSchemes {
    pub(crate) bindings: Vec<(String, Option<Closed>)>,
    /// Installed handler names, used only to word diagnostics.
    pub(crate) handled: Vec<String>,
    pub(crate) builtins: Manifest,
}

impl SessionSchemes {
    /// No bindings or handled names: only Σ.
    pub fn new(builtins: Manifest) -> Self {
        Self {
            bindings: Vec::new(),
            handled: Vec::new(),
            builtins,
        }
    }

    /// Seed from the baked prelude's own schemes, against a host surface's
    /// manifest, for compile-only callers with no live shell, including tests.
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

/// What this unit is checked to produce.
#[derive(Clone, Copy)]
pub enum Unit {
    Script,
    Host(&'static Table),
    Module,
}

fn seeded_generator(schemes: SessionSchemes) -> Generator {
    let mut solver = Solver::new();
    let (env, residuals) = solver.seed(schemes);
    Generator::new(solver, env, residuals)
}

/// Exit the inference region before generalising, and export only after closure.
fn close_thunk_scheme(generator: Generator, cty: CompTy) -> Result<Closed, Vec<TypeError>> {
    let scheme = generator.solver.generalise(&Ty::thunk(cty));
    let solution = generator.solver.close()?;
    Ok(solution.settle(Arc::new(scheme)))
}

/// Type-check `phrases`, seeding from the live session.
///
/// Infer each phrase in order, building its checked IR beside its type and
/// extending `TyEnv` at each `Define`. After closure succeeds, freeze sites,
/// settle `Define` schemes and build readmissions under one fresh unit. Only closed
/// schemes leave — a run's unifier dies with the run and its variable ids
/// restart at zero, so an open scheme from run *N* would alias run
/// *N+1*'s fresh variables.
///
/// `unit` selects a script, host options table, or closed module.
///
/// # Errors
/// Every diagnostic inference collected, whenever that list is non-empty.
/// Erroneous units are refused whole and their inferred IR is dropped without freezing.
pub fn typecheck(
    phrases: &[Spanned<Phrase<()>>],
    schemes: SessionSchemes,
    unit: Unit,
) -> Result<Toplevel, Vec<TypeError>> {
    let mut generator = seeded_generator(schemes);
    let options = match unit {
        Unit::Host(table) => Some(table),
        Unit::Script | Unit::Module => None,
    };
    let pending = generator.phrases(phrases, options);
    let solution = generator.solver.close()?;
    if matches!(unit, Unit::Module) {
        let mut errors = undecided_exports(&solution, &pending);
        if !errors.is_empty() {
            sort_errors(&mut errors);
            return Err(errors);
        }
    }
    Ok(
        Readback::new(&solution, UnitId::fresh(), generator.synth_counter)
            .toplevel(&pending, &generator.residual_uses),
    )
}

fn undecided_exports(
    solution: &Solution,
    pending: &[Spanned<Phrase<generate::LiveSchemes, env::Inferring>>],
) -> Vec<TypeError> {
    crate::ir::public_exports(pending)
        .into_iter()
        .filter_map(|(name, (scheme, span))| {
            let (scheme, source) = solution.undecided_export(scheme)?;
            Some(TypeError {
                pos: span,
                kind: TypeErrorKind::UndecidedExport {
                    name: name.into(),
                    scheme,
                    source,
                },
                reason: None,
                weak: None,
                unit: None,
            })
        })
        .collect()
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
    match typecheck(phrases, SessionSchemes::new(manifest.clone()), Unit::Script) {
        Ok(annotated) => annotated,
        Err(errs) => {
            let msgs: Vec<String> = errs.iter().map(ToString::to_string).collect();
            panic!("prelude type errors:\n{}", msgs.join("\n"));
        }
    }
}

/// Decide whether `comp` is a handler for `name`.
///
/// # Errors
/// The handler takes other than an argv, or produces other than `Command`.
pub(crate) fn arm_stands_in(
    name: &str,
    comp: &Arc<Comp>,
    schemes: SessionSchemes,
) -> Result<(), Vec<TypeError>> {
    let mut generator = seeded_generator(schemes);
    generator.region(|this| this.arm(&Val::thunk(Arc::clone(comp)), Arm::Named(name), None));
    generator.solver.close().map(drop)
}

/// The scheme for a hook's captured closure (`Shell::register_hook`):
/// the closure's comp inferred as written, closed against its own unifier.
pub(crate) fn binding_value_scheme(
    body: &Comp,
    schemes: SessionSchemes,
) -> Result<Closed, Vec<TypeError>> {
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

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::compile::{CompileError, compile_and_typecheck};
    use crate::source::FileId;

    fn check(source: &str) -> Result<Toplevel, CompileError> {
        compile_and_typecheck(
            source,
            crate::test_helper::core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Module,
            &mut crate::compile::no_modules,
        )
    }

    #[test]
    fn modules_refuse_each_public_undecided_export_at_its_definition() {
        let source =
            "let config = echo 3 | from-json; let decode = { |s| to-string $s | from-json }";
        let Err(CompileError::Types(errors)) = check(source) else {
            panic!("undecided exports must be refused");
        };
        assert_eq!(errors.len(), 2, "{errors:?}");
        for (error, name) in errors.iter().zip(["config", "decode"]) {
            assert!(
                matches!(&error.kind, TypeErrorKind::UndecidedExport { name: found, .. } if found == name)
            );
            let pos = error.pos.expect("the definition's span");
            assert!(source[pos.start as usize..pos.end as usize].contains(name));
            let TypeErrorKind::UndecidedExport {
                scheme,
                source:
                    WeakSource::Boundary {
                        name: boundary,
                        span: Some(span),
                    },
                ..
            } = &error.kind
            else {
                panic!("the export must retain its boundary call: {error:?}");
            };
            assert!(scheme.has_weak(), "{scheme}");
            let shown = scheme.to_string();
            assert!(
                shown.contains('_'),
                "weak variables retain their marker: {shown}"
            );
            assert!(error.kind.render_message().contains(&shown), "{error}");
            assert_eq!(&**boundary, "from-json");
            assert!(source[span.start as usize..span.end as usize].contains("from-json"));
            let report = error.report();
            assert_eq!(report.at.unwrap().span, Some(pos));
            assert_eq!(report.also.unwrap().span, Some(*span));
            assert!(error.hint().unwrap().contains("export the text"));
        }
    }

    #[test]
    fn module_exports_are_judged_after_local_uses_and_private_names_are_ignored() {
        for source in [
            "let config = echo 3 | from-json; let _n = return $[$config + 1]",
            "let _config = echo 3 | from-json",
            "let id = { |x| return $x }",
            "let config = echo 3 | from-json; let config = 42",
            "let decode = { |s| to-string $s | from-json }; let decode = { |x| return $x }",
        ] {
            check(source).unwrap_or_else(|error| panic!("{source}: {error}"));
        }
    }

    #[test]
    fn only_the_final_undecided_binding_is_judged_at_its_own_span() {
        let source = "let config = echo 1 | from-json; let config = echo 2 | from-json";
        let Err(CompileError::Types(errors)) = check(source) else {
            panic!("the final undecided export must be refused");
        };
        let [error] = errors.as_slice() else {
            panic!("only the final definition is exported: {errors:?}");
        };
        let pos = error.pos.expect("the winning definition's span");
        assert!(source[pos.range()].contains("echo 2"));
        let TypeErrorKind::UndecidedExport {
            name,
            source: WeakSource::Boundary {
                span: Some(span), ..
            },
            ..
        } = &error.kind
        else {
            panic!("the final export retains its boundary: {error:?}");
        };
        assert_eq!(name, "config");
        assert!(span.start > u32::try_from(source.find(';').unwrap()).unwrap());
    }

    #[test]
    fn failed_closure_does_not_add_export_errors() {
        let Err(CompileError::Types(errors)) =
            check("let config = echo 3 | from-json; return $[1 + 'bad']")
        else {
            panic!("the arithmetic must be refused");
        };
        assert!(
            !errors
                .iter()
                .any(|error| matches!(error.kind, TypeErrorKind::UndecidedExport { .. }))
        );
    }
}
