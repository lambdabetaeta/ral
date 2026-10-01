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
            Phrase::Run(_) => Vec::new(),
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
    for expected in ["words", "reverse", "for"] {
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
/// the bare elaborated toplevel, none would exist anywhere in the tree.  A
/// focused fixture binds an external's bytes as an argument.
#[test]
fn bake_inserts_an_interior_capture() {
    let ast = ral_core::syntax::parser::parse("let count = { |p| int !{wc -l < $p} }")
        .expect("fixture parse");
    let top = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
        .expect("elaborate");
    let (annotated, _) = ral_core::bake_prelude(&top);
    let mut capture = false;
    walk_toplevel(&annotated, &mut |c| {
        if let CompKind::Capture(_) = &c.item {
            capture = true;
        }
    });
    assert!(
        capture,
        "a bound external must carry a Capture node — the bake's checked pass inserts it"
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

/// The prelude's producers carry the grades the design gives them: a
/// wrapper that runs a block produces what the block produces, a wrapper
/// that binds a value demands one, and a stream combinator's callback may
/// be a command.
#[test]
fn prelude_schemes_carry_their_grades() {
    let (_, schemes) = rebake();
    let shown = |name: &str| {
        let (_, scheme) = schemes
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("the prelude binds {name}"));
        fmt_scheme(scheme)
    };
    assert_eq!(shown("retry"), "∀α ν. Integer → {ν α} → ν α");
    assert_eq!(shown("attempt"), "∀α ν. {ν α} → Returns Unit");
    assert_eq!(shown("succeeds"), "∀α ν. {ν α} → Returns Bool");
    assert_eq!(shown("for"), "∀α β ν. [α] → {α → ν β} → Returns Unit");
    assert_eq!(
        shown("par"),
        "∀α β. {α → Returns β} → [α] → Integer → Returns [β]"
    );
    assert_eq!(
        shown("map-lines"),
        "∀α. {String → Returns α} → Returns Unit"
    );
    assert_eq!(
        shown("filter-lines"),
        "{String → Returns Bool} → Returns Unit"
    );
    assert_eq!(shown("each-line"), "∀α ν. {String → ν α} → Returns Unit");
    assert_eq!(shown("bytes-to-string"), "Bytes → Returns String");
    assert!(
        shown("defer").starts_with("∀α ν. {ν α} → Returns Handle [outcome: [`ok: α | `err: "),
        "{}",
        shown("defer")
    );
}

/// The builtin rows the design grades: writers are commands, the runners of
/// blocks absorb any grade, `map` demands a value, `fail` inhabits every
/// producer type.
#[test]
fn builtin_schemes_carry_their_grades() {
    let table = ral_core::HostSurface::default().builtin_table();
    let shown = |name: &str| ral_core::typecheck::builtin_type_hint(&table, name).unwrap();
    assert_eq!(shown("echo"), "[String] → Command");
    assert_eq!(shown("to-json"), "∀α:data. α → Command");
    assert_eq!(shown("spawn"), "∀α ν. {ν α} → Returns Handle α");
    assert_eq!(shown("each"), "∀α β ν. {α → ν β} → [α] → Returns Unit");
    assert_eq!(shown("map"), "∀α β. {α → Returns β} → [α] → Returns [β]");
    assert_eq!(
        shown("fold-lines"),
        "∀α ν. {α → String → ν α} → α → Returns α"
    );
    assert_eq!(
        shown("fail"),
        "∀α ν ρ. [status: Integer, message: String, ...ρ] → ν α"
    );
}
