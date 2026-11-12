//! Regression test for the prelude bake.
//!
//! The prelude elaborates to unchecked phrases, one per top-level statement;
//! `bake_prelude` checks them in order and returns a [`ral_core::ir::Toplevel`],
//! each `Phrase::Define` carrying its settled schemes. This test pins that
//! they reach every top-level `let`, and that interior checker-built nodes
//! (`Capture`) land on the baked tree.

use ral_core::ir::{CompKind, Phrase, Toplevel};

/// Re-bake the prelude from source so the test owns the annotated toplevel.
fn rebake() -> Toplevel {
    let src = include_str!("../../../../src/shell/prelude.ral");
    let ast = ral_core::syntax::parser::parse(src).expect("prelude parse");
    let top = ral_core::elaborator::elaborate(&ast, [], None, &mut crate::common::no_modules)
        .expect("elaborate");
    ral_core::bake_prelude(&top, &ral_core::HostSurface::default().manifest())
}

/// The (name, scheme) pairs on an annotated toplevel's `Phrase::Define`s, in
/// phrase order.
fn schemes_on_defines(top: &Toplevel) -> Vec<(String, ral_core::ty::Closed)> {
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
    assert!(
        !names.contains("is-done"),
        "is-done is a native builtin and must not remain in the baked prelude"
    );
}

/// A prelude binding sharing a native's name seeds and shadows, so the
/// checker and the env-first runtime agree.  A synthetic fixture stands in —
/// the real prelude names nothing that collides.
#[test]
fn a_prelude_binding_colliding_with_a_native_survives_the_harvest() {
    let ast =
        ral_core::syntax::parser::parse("let upper = { |x| return $x }").expect("fixture parse");
    let top = ral_core::elaborator::elaborate(&ast, [], None, &mut crate::common::no_modules)
        .expect("elaborate");
    let schemes = schemes_on_defines(&ral_core::bake_prelude(
        &top,
        &ral_core::HostSurface::default().manifest(),
    ));
    assert!(
        schemes.iter().any(|(name, _)| name == "upper"),
        "a prelude binding named after a native must survive the harvest, got {schemes:?}"
    );
}

/// The bake parses, elaborates and typechecks: inference constructs the IR,
/// then freeze snapshots sites and settles schemes. The embedded blob carries
/// checked interior nodes, not just each phrase's own root.  The
/// elaborator never emits a `Capture` node, so one below a phrase's root is
/// proof the checked pass descended and inserted it — were the bake to embed
/// the bare elaborated toplevel, none would exist anywhere in the tree.  A
/// focused fixture binds an external's bytes as an argument.
#[test]
fn bake_inserts_an_interior_capture() {
    let ast = ral_core::syntax::parser::parse("let count = { |p| int !{wc -l < $p} }")
        .expect("fixture parse");
    let top = ral_core::elaborator::elaborate(&ast, [], None, &mut crate::common::no_modules)
        .expect("elaborate");
    let annotated = ral_core::bake_prelude(&top, &ral_core::HostSurface::default().manifest());
    let mut capture = false;
    crate::common::walk_toplevel(&annotated, &mut |c| {
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
    // Fresh grades print only where unit is possible (SPEC §17.3).
    assert_eq!(
        shown("retry"),
        "∀α ν. Integer → {Command[ν] α} → Command[ν] α"
    );
    assert_eq!(shown("attempt"), "∀α ν ξ. {Command[ν] α} → Command[ξ] Unit");
    assert_eq!(shown("succeeds"), "∀α ν. {Command[ν] α} → Returns Bool");
    assert_eq!(
        shown("for"),
        "∀α β ν ξ. [α] → {α → Command[ν] β} → Command[ξ] Unit"
    );
    assert_eq!(
        shown("map-lines"),
        "∀α ν. {String → Returns α} → Command[ν] Unit"
    );
    assert_eq!(
        shown("filter-lines"),
        "∀ν. {String → Returns Bool} → Command[ν] Unit"
    );
    assert_eq!(
        shown("each-line"),
        "∀α ν ξ. {String → Command[ξ] α} → Command[ν] Unit"
    );
    assert_eq!(shown("bytes-to-string"), "Bytes → Returns String");
    // `par` replays its callback's output rather than capturing it.
    assert_eq!(
        shown("par"),
        "∀α β ν. {α → Command[ν] β} → [α] → Integer → Returns [β]"
    );
    assert_eq!(
        shown("both"),
        "∀α β. {Returns α} → {Returns β} → Returns [fst: α, snd: β]"
    );
    assert_eq!(shown("first-of"), "∀α ν. [{Command[ν] α}] → Command[ν] α");
}

/// The builtin rows the design grades: writers are commands, the runners of
/// blocks absorb any grade, a handle carries its block's grade and `await`
/// returns it, `map` accepts either grade, `fail` inhabits every producer type.
#[test]
fn builtin_schemes_carry_their_grades() {
    let table = ral_core::HostSurface::default().manifest();
    let shown = |name: &str| ral_core::typecheck::builtin_type_hint(&table, name).unwrap();
    assert_eq!(shown("echo"), "[String] → Command");
    assert_eq!(shown("to-json"), "∀α:data. α → Command");
    // Visible grade variables use `Command[ν]` (SPEC §17.3).
    assert_eq!(shown("await"), "∀α ν. Handle (Command[ν] α) → Command[ν] α");
    assert_eq!(
        shown("is-done"),
        "∀α ν. Handle (Command[ν] α) → Returns Bool"
    );
    assert_eq!(
        shown("each"),
        "∀α β ν ξ. {α → Command[ν] β} → [α] → Command[ξ] Unit"
    );
    // `map { |f| echo $f }` requires a grade-polymorphic callback.
    assert_eq!(
        shown("map"),
        "∀α β ν. {α → Command[ν] β} → [α] → Returns [β]"
    );
    assert_eq!(
        shown("fold-lines"),
        "∀α ν ξ. {α → String → Command[ν] α} → α → Command[ξ] α"
    );
    assert_eq!(shown("fail"), "∀α ν. String → Command[ν] α");
    // No printed quantifier for a grade paired with a non-unit type.
    assert_eq!(shown("cd"), "∀ν. String → Command[ν] Unit");
    assert_eq!(shown("length"), "∀α:sized. α → Returns Integer");
}
