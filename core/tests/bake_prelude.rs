//! Regression test for the prelude bake.
//!
//! The prelude elaborates to a top-level `Seq`, and ordinary inference
//! of a `Seq` runs inside a fresh `TyEnv` frame so that lets inside a
//! `{…}` block don't leak past it.  `bake_prelude` checks the prelude's
//! parts in the root scope instead, so its top-level lets survive into
//! the harvested scheme list.  A path through the generic `infer_comp`
//! would *pop* every prelude binding before the harvest could collect
//! them — the returned `Vec` was empty, and exarch's system prompt
//! rendered the `# Prelude reference (authoritative)` header with no
//! body.  This test pins the post-fix behaviour.

mod common;

use ral_core::ir::{CompKind, IrPattern};
use ral_core::typecheck::fmt_scheme;

/// Re-bake the prelude from source so the test owns both the annotated
/// comp and the schemes harvested off its `Bind` nodes.
fn rebake() -> (ral_core::Comp, Vec<(String, ral_core::Scheme)>) {
    let src = include_str!("../src/prelude.ral");
    let ast = ral_core::parse(src).expect("prelude parse");
    let comp = ral_core::elaborate(&ast, Default::default());
    ral_core::bake_prelude(&comp)
}

/// Read the (name, scheme) pairs off an annotated comp's top-level `Bind`
/// nodes — the spine the harvest walks (a `Seq`'s parts and a `Bind`'s
/// `rest`).  Builtin names are filtered exactly as `bake_prelude` does.
fn schemes_on_binds(comp: &ral_core::Comp) -> Vec<(String, String)> {
    let mut out = Vec::new();
    fn walk(comp: &ral_core::Comp, out: &mut Vec<(String, String)>) {
        match &comp.item {
            CompKind::Seq(parts) => {
                for part in parts {
                    walk(part, out);
                }
            }
            CompKind::Bind {
                pattern: IrPattern::Name(name),
                rest,
                scheme: Some(scheme),
                ..
            } => {
                if !ral_core::builtins::is_builtin(name) {
                    out.push((name.clone(), fmt_scheme(scheme)));
                }
                walk(rest, out);
            }
            CompKind::Bind { rest, .. } => walk(rest, out),
            _ => {}
        }
    }
    walk(comp, &mut out);
    out
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

#[test]
fn bake_filters_builtins() {
    let (_, schemes) = rebake();
    for (name, _) in &schemes {
        assert!(
            !ral_core::builtins::is_builtin(name),
            "baked prelude schemes must not shadow builtin {name:?}"
        );
    }
}

/// The bake runs one checked pass — parse, elaborate, annotate — so the
/// comp blob the build embeds already carries the checker's ground mode
/// verdicts on interior nodes, not just the top-level spine.  The
/// elaborator emits an all-`Empty` placeholder, so a `Bytes` mode below
/// the spine is proof the checked pass descended and overwrote it — were
/// the bake to embed the bare elaborated comp, every slot would stay
/// `Empty`.
///
/// The streaming reducers (`map-lines` / `filter-lines` / `each-line`) wrap
/// an `echo`-per-line body, a byte-emitting `Bind` whose `rhs_output` the
/// bake annotates `Bytes`.
#[test]
fn baked_prelude_carries_interior_rhs_modes() {
    use ral_core::mode::ByteMode;
    let (annotated, _) = rebake();
    let mut rhs = false;
    common::walk_comp(&annotated, &mut |c| {
        if let CompKind::Bind {
            rhs_output: ByteMode::Bytes,
            ..
        } = &c.item
        {
            rhs = true;
        }
    });
    assert!(
        rhs,
        "a prelude bind must carry a Bytes rhs_output — the bake's checked pass annotates it"
    );
}

/// The other interior annotation: a `Pipeline`'s `Bytes` wires.  The core
/// prelude has no `|` pipeline of its own (the hashed `view` lives in
/// exarch's `agent.ral`), so a focused fixture stands in — the bake path is
/// identical, and the probe no longer hinges on incidental prelude content.
#[test]
fn bake_annotates_interior_pipeline_wires() {
    use ral_core::mode::ByteMode;
    let ast = ral_core::parse("let tag = { cat -n | head -n 1 }").expect("fixture parse");
    let comp = ral_core::elaborate(&ast, Default::default());
    let (annotated, _) = ral_core::bake_prelude(&comp);
    let mut wires = false;
    common::walk_comp(&annotated, &mut |c| {
        if let CompKind::Pipeline { wires: ws, .. } = &c.item
            && ws
                .iter()
                .any(|w| w.output == ByteMode::Bytes || w.input == ByteMode::Bytes)
        {
            wires = true;
        }
    });
    assert!(
        wires,
        "a byte pipeline must carry a Bytes wire — the bake's checked pass annotates it"
    );
}

/// The schemes `bake_prelude` returns are the ones written onto the
/// annotated comp's `Bind` nodes — one `Bind`-node harvest, not a
/// separate `TyEnv` walk.  Comparing the comp's binds to the returned
/// list within a *single* bake renders each scheme identically; an
/// independent second bake would alpha-rename the quantified variables,
/// so the comparison must stay inside one unifier run.
#[test]
fn annotated_binds_carry_the_harvested_schemes() {
    let (annotated, schemes) = rebake();
    let on_binds = schemes_on_binds(&annotated);
    let returned: Vec<(String, String)> = schemes
        .iter()
        .map(|(n, s)| (n.clone(), fmt_scheme(s)))
        .collect();
    assert_eq!(
        on_binds, returned,
        "the returned schemes must be exactly the ones on the annotated comp's Bind nodes"
    );
}
