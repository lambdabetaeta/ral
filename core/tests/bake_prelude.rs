//! Regression test for the prelude bake.
//!
//! The prelude elaborates to a [`ral_core::ir::Toplevel`], one `Phrase` per
//! top-level statement; `bake_prelude` checks the phrases in order and
//! harvests each `Phrase::Define`'s generalised schemes straight off it — no
//! separate `TyEnv` walk.  This test pins that the harvest actually reaches
//! every top-level `let`, and that the checked pass's interior annotations
//! (`Capture`, a pipeline's yield) land on the baked tree.

mod common;

use ral_core::ir::{CompKind, Phrase, Toplevel};
use ral_core::typecheck::fmt_scheme;

/// Re-bake the prelude from source so the test owns both the annotated
/// toplevel and the schemes harvested off its `Phrase::Define`s.
fn rebake() -> (Toplevel, Vec<(String, ral_core::Scheme)>) {
    let src = include_str!("../src/prelude.ral");
    let ast = ral_core::syntax::parser::parse(src).expect("prelude parse");
    let top = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
        .expect("elaborate");
    ral_core::bake_prelude(&top)
}

/// Visit every `Comp` reachable from an annotated toplevel's phrases —
/// [`common::walk_comp`] from each phrase's own root.
fn walk_toplevel(top: &Toplevel, visit: &mut impl FnMut(&ral_core::ir::Comp)) {
    for phrase in &top.phrases {
        match &phrase.item {
            Phrase::Source { path } => common::walk_comp(path, visit),
            Phrase::Define { comp, .. } | Phrase::Run(comp) => common::walk_comp(comp, visit),
        }
    }
}

/// Read the (name, scheme) pairs off an annotated toplevel's
/// `Phrase::Define`s, in phrase order — the harvest `bake_prelude` performs.
fn schemes_on_defines(top: &Toplevel) -> Vec<(String, String)> {
    top.phrases
        .iter()
        .flat_map(|phrase| match &phrase.item {
            Phrase::Define { schemes, .. } => schemes
                .iter()
                .map(|(name, scheme)| (name.clone(), fmt_scheme(scheme)))
                .collect(),
            Phrase::Source { .. } | Phrase::Run(_) => Vec::new(),
        })
        .collect()
}

#[test]
fn bake_returns_top_level_let_bindings() {
    let (_, schemes) = rebake();
    let names: std::collections::HashSet<&str> = schemes.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        !schemes.is_empty(),
        "expected the prelude's top-level lets to be visible after baking, got an empty Vec"
    );
    for expected in ["lines", "words", "reverse", "for"] {
        assert!(
            names.contains(expected),
            "expected baked prelude schemes to include {expected:?}, got {names:?}"
        );
    }
}

/// A prelude binding sharing a native's name seeds and shadows, so the
/// checker and the env-first runtime agree.  A synthetic fixture stands in —
/// the real prelude names nothing that collides.
#[test]
fn a_prelude_binding_colliding_with_a_native_survives_the_harvest() {
    let ast =
        ral_core::syntax::parser::parse("let upper = { |x| return $x }").expect("fixture parse");
    let top = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
        .expect("elaborate");
    let (_, schemes) = ral_core::bake_prelude(&top);
    assert!(
        schemes.iter().any(|(name, _)| name == "upper"),
        "a prelude binding named after a native must survive the harvest, got {schemes:?}"
    );
}

/// The bake runs one checked pass — parse, elaborate, annotate — so the
/// toplevel blob the build embeds already carries the checker's ground
/// verdicts on interior nodes, not just each phrase's own root.  The
/// elaborator never emits a `Capture` node, so one below a phrase's root is
/// proof the checked pass descended and inserted it — were the bake to embed
/// the bare elaborated toplevel, none would exist anywhere in the tree.
///
/// The streaming reducers (`map-lines` / `filter-lines` / `each-line`) wrap
/// an `echo`-per-line body, whose byte-payload bind RHS the bake wraps in
/// `Capture`.
#[test]
fn baked_prelude_carries_interior_captures() {
    let (annotated, _) = rebake();
    let mut capture = false;
    walk_toplevel(&annotated, &mut |c| {
        if let CompKind::Capture(_) = &c.item {
            capture = true;
        }
    });
    assert!(
        capture,
        "a prelude bind must carry a Capture node — the bake's checked pass inserts it"
    );
}

/// The other interior annotation: a `Pipeline`'s yield.  The core
/// prelude has no `|` pipeline of its own (the hashed `view` lives in
/// exarch's `agent.ral`), so a focused fixture stands in — the bake path is
/// identical, and the probe no longer hinges on incidental prelude content.
#[test]
fn bake_annotates_a_pipelines_yield() {
    use ral_core::ir::PipeYield;
    let ast =
        ral_core::syntax::parser::parse("let tag = { cat -n | head -n 1 }").expect("fixture parse");
    let top = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
        .expect("elaborate");
    let (annotated, _) = ral_core::bake_prelude(&top);
    let mut yields = Vec::new();
    walk_toplevel(&annotated, &mut |c| {
        if let CompKind::Pipeline { yields: y, .. } = &c.item {
            yields.push(*y);
        }
    });
    assert_eq!(
        yields,
        vec![PipeYield::Unit],
        "an external tail is captured from stdout — the bake's checked pass writes that yield"
    );
}

/// The schemes `bake_prelude` returns are the ones written onto the
/// annotated toplevel's `Phrase::Define`s — one harvest, not a separate
/// `TyEnv` walk.  Comparing the toplevel's defines to the returned list
/// within a *single* bake renders each scheme identically; an independent
/// second bake would alpha-rename the quantified variables, so the
/// comparison must stay inside one unifier run.
#[test]
fn annotated_binds_carry_the_harvested_schemes() {
    let (annotated, schemes) = rebake();
    let on_defines = schemes_on_defines(&annotated);
    let returned: Vec<(String, String)> = schemes
        .iter()
        .map(|(n, s)| (n.clone(), fmt_scheme(s)))
        .collect();
    assert_eq!(
        on_defines, returned,
        "the returned schemes must be exactly the ones on the annotated toplevel's Phrase::Define"
    );
}
