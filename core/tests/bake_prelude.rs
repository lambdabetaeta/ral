//! Regression test for the prelude bake.
//!
//! The prelude elaborates to a [`ral_core::ir::Toplevel`], one `Phrase` per
//! top-level statement; `bake_prelude` checks the phrases in order, each
//! `Phrase::Define` carrying its generalised schemes.  This test pins that
//! they reach every top-level `let`, and that the checked pass's interior
//! annotations (`Capture`, a pipeline's yield) land on the baked tree.

mod common;

use ral_core::ir::{CompKind, Phrase, Toplevel};

/// Re-bake the prelude from source so the test owns the annotated toplevel.
fn rebake() -> Toplevel {
    let src = include_str!("../src/prelude.ral");
    let ast = ral_core::syntax::parser::parse(src).expect("prelude parse");
    let top = ral_core::elaborator::elaborate(&ast, [], "").expect("elaborate");
    ral_core::bake_prelude(&top, &common::manifest(&ral_core::HostSurface::default()))
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

/// The (name, scheme) pairs on an annotated toplevel's `Phrase::Define`s, in
/// phrase order.
fn schemes_on_defines(top: &Toplevel) -> Vec<(String, std::sync::Arc<ral_core::Scheme>)> {
    top.phrases
        .iter()
        .flat_map(|phrase| match &phrase.item {
            Phrase::Define { schemes, .. } => schemes.clone(),
            Phrase::Run(_) => Vec::new(),
        })
        .collect()
}

#[test]
fn bake_returns_top_level_let_bindings() {
    let schemes = schemes_on_defines(&rebake());
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
    let top = ral_core::elaborator::elaborate(&ast, [], "").expect("elaborate");
    let schemes = schemes_on_defines(&ral_core::bake_prelude(
        &top,
        &common::manifest(&ral_core::HostSurface::default()),
    ));
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
    let top = ral_core::elaborator::elaborate(&ast, [], "").expect("elaborate");
    let annotated =
        ral_core::bake_prelude(&top, &common::manifest(&ral_core::HostSurface::default()));
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

/// The prelude's producers carry the grades the design gives them: a
/// wrapper that runs a block produces what the block produces, a wrapper
/// that binds a value demands one, and a stream combinator's callback may
/// be a command.
#[test]
fn prelude_schemes_carry_their_grades() {
    let schemes = schemes_on_defines(&rebake());
    let shown = |name: &str| {
        let (_, scheme) = schemes
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("the prelude binds {name}"));
        scheme.to_string()
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
    let table = common::manifest(&ral_core::HostSurface::default());
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
