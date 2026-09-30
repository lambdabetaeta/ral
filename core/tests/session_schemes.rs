//! The ADR session-scheme-continuity Verify list, exercised end-to-end.
//!
//! Each run's static check is seeded from the live session: the schemes
//! the checker inferred for run *N*'s top-level binds live on the runtime
//! bindings (and the alias arms' schemes on the persistent handler frames),
//! so run *N+1*'s check sees them.  The harness mirrors the REPL loop in
//! `ral/src/repl/exec.rs`: `check` seeds `compile_and_typecheck` from the
//! live `session_schemes()`, and `run` drives the public `run` door.

mod common;

use ral_core::protocol::{Program, Run};
use ral_core::source::FileId;
use ral_core::types::{GrantStack, Settled};
use ral_core::{
    CompileError, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Shell,
    TypeError, Value, builtins, compile_and_typecheck, typecheck::fmt_scheme,
};
use std::sync::Arc;

fn shell() -> Shell {
    let mut s = Shell::default();
    builtins::register(&mut s, common::prelude_comp());
    s
}

/// The scheme `name` carries on the live scope, `None` when it is unbound
/// or bound without one.
fn scheme_of(sh: &Shell, name: &str) -> Option<Arc<ral_core::typecheck::Scheme>> {
    sh.binding_schemes()
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, scheme)| scheme)
}

/// One REPL run through the public `run` door, which checks `src`
/// against the live session before evaluating it.  Panics on parse / type
/// failure — callers that expect a clean run pick source that compiles;
/// callers probing an *eval* failure get the body's `Settled` back.
fn run(shell: &mut Shell, src: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

/// Check `src` against the live session without evaluating it — the
/// errors a run would surface before running.
fn check_errors(shell: &Shell, src: &str) -> Vec<TypeError> {
    match compile_and_typecheck(src, shell.session_schemes(), FileId::DUMMY, "", None) {
        Ok(_) => Vec::new(),
        Err(CompileError::Parse(e)) => panic!("parse: {src:?}: {e}"),
        Err(CompileError::Types(errs)) => errs,
    }
}

// ─── (1) a value producer feeds nothing down a pipe ─────────────────────────

/// `let f = { return 3 }` then `f | from-json`: a stage feeds the next by
/// writing, and `f` returns an Int.  The next run's check refuses it, with the
/// binding's session scheme as the seed.
#[test]
fn value_producer_into_decoder_is_refused_cross_run() {
    let mut sh = shell();
    run(&mut sh, "let f = { return 3 }").unwrap();
    let errs = check_errors(&sh, "f | from-json");
    assert_eq!(
        errs.iter().map(|e| e.kind.code()).collect::<Vec<_>>(),
        ["T0011"]
    );
}

// ─── (2) byte producer into byte consumer typechecks via harvested scheme ────

/// `let f = { echo hi }` then `f | wc -l`: the harvested scheme for `f`
/// is `{Returns Unit}`, a command, so it may feed `wc`'s input.
#[test]
fn byte_producer_into_byte_consumer_typechecks() {
    let mut sh = shell();
    run(&mut sh, "let f = { echo hi }").unwrap();
    assert!(
        check_errors(&sh, "f | wc -l").is_empty(),
        "expected a clean byte→byte pipeline across runs, got: {:?}",
        check_errors(&sh, "f | wc -l")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (3) cross-run value-type error ─────────────────────────────────────────

/// A run-*N* binding used at a clashing value type in run *N+1* is
/// reported statically: `x` is `String` (the literal `'hello'`), so
/// `$x + 1` is a String where a number is needed.  The suite's established clash
/// is `String + Int` (see `typecheck.rs`), not the `Int + "hi"` the ADR
/// sketches — the language has no string literal inside `$[…]`.
#[test]
fn cross_run_value_type_error_is_static() {
    let mut sh = shell();
    run(&mut sh, "let xv = 'hello'").unwrap();
    let errs = check_errors(&sh, "return $[$xv + 1]");
    assert!(
        errs.iter().any(|e| e.kind.code() == "T0074"),
        "expected a cross-run value-type mismatch, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (4) rebind retypes the name ─────────────────────────────────────────────

/// `let x = 3` then `let x = 'hello'`: the rebind replaces the scheme,
/// so the next run checks `$x` against `String`.  `$[$x + 1]` is then a
/// mismatch, while passing `$x` to the String-consuming builtin `upper`
/// is clean.  The bare `x = …` assignment form the ADR sketches parses
/// as a command application, not a rebind, so the rebind is spelled
/// `let`; the language has no `++` concatenation operator, so the clean
/// String use is `upper`.
#[test]
fn rebind_retypes_the_name() {
    let mut sh = shell();
    run(&mut sh, "let xv = 3").unwrap();
    run(&mut sh, "let xv = 'hello'").unwrap();
    let errs = check_errors(&sh, "return $[$xv + 1]");
    assert!(
        errs.iter().any(|e| e.kind.code() == "T0074"),
        "expected $xv (now String) + 1 to mismatch, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    assert!(
        check_errors(&sh, "!{upper $xv}").is_empty(),
        "expected String use of $xv to be clean, got: {:?}",
        check_errors(&sh, "!{upper $xv}")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (5) a failed statement installs nothing ─────────────────────────────────

/// `let x = 1` then a run `nonexistent-command-zz; let x = hello` that
/// fails before the rebind: `$x` keeps the `Int` scheme.  The failed
/// statement installed neither value nor scheme.  The rebind is spelled
/// `let` (bare `x = …` is a command application, not a rebind); the eval
/// run is expected to error, and the binding must survive unchanged.
#[test]
fn failed_statement_installs_no_scheme() {
    let mut sh = shell();
    run(&mut sh, "let xv = 1").unwrap();
    // The bad command aborts the run before the rebind runs.
    let outcome = run(&mut sh, "nonexistent-command-zz\nlet xv = hello");
    assert!(outcome.is_err(), "the bad command must abort the run");
    // `xv` is still Int, so Int arithmetic is clean.
    assert!(
        check_errors(&sh, "return $[$xv + 1]").is_empty(),
        "expected $xv to keep its Int scheme after the failed rebind, got: {:?}",
        check_errors(&sh, "return $[$xv + 1]")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (6) pattern binds generalise their own scheme ───────────────────────────

/// `let [a, b] = [1, 2]` then `$a + 1`: cross-run use of a pattern-bound
/// name neither errors spuriously nor loses its scheme — each destructured
/// component generalises from the component type the pattern reaches, one
/// scheme per bound name, just as a plain `let` binding does.
#[test]
fn pattern_binds_generalise_their_own_scheme() {
    let mut sh = shell();
    run(&mut sh, "let [a, bb] = [1, 2]").unwrap();
    assert!(
        check_errors(&sh, "return $[$a + 1]").is_empty(),
        "expected pattern-bound $a to check cleanly, got: {:?}",
        check_errors(&sh, "return $[$a + 1]")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    assert!(
        scheme_of(&sh, "a").is_some(),
        "a pattern-bound name must generalise its own scheme"
    );
    // A plain `let` binding does carry a scheme.
    run(&mut sh, "let cnt = 3").unwrap();
    assert!(
        scheme_of(&sh, "cnt").is_some(),
        "a plain let binding must carry a scheme"
    );
}

// ─── (7) alias visibility ────────────────────────────────────────────────────

/// `alias three { |args| echo 3 }` is visible to the next run's check: the
/// arm's handler scheme persists, so `three` alone is clean and `$three`
/// (first-classing a handler) is a static error.  After `unalias three` the name
/// falls back to external typing, so `$three` is an unbound variable, refused
/// as such rather than as a handler entry.
#[test]
fn alias_visible_to_next_run() {
    let mut sh = shell();
    run(&mut sh, "alias three { |args| echo 3 }").unwrap();
    assert!(
        check_errors(&sh, "three").is_empty(),
        "expected the alias used alone to be clean, got: {:?}",
        check_errors(&sh, "three")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    let errs = check_errors(&sh, "return $three");
    assert!(
        errs.iter()
            .any(|e| e.kind.render_message().contains("handler entry")),
        "expected the persisted alias scheme to reject `$three` as a handler entry, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    run(&mut sh, "unalias three").unwrap();
    let codes: Vec<_> = check_errors(&sh, "return $three")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(
        codes,
        ["T0071"],
        "after unalias `$three` is merely unbound, not a handler entry"
    );
}

// ─── (8) bake/run harvest unity ─────────────────────────────────────────────

/// The scheme installed on a live scope binding renders identically to
/// the baked entry harvested at build time — both come from the same
/// `Bind`-node harvest.
#[test]
fn live_binding_scheme_matches_baked_entry() {
    let sh = shell();
    let baked: std::collections::HashMap<&str, String> = common::prelude_schemes()
        .iter()
        .map(|(n, s)| (n.as_str(), fmt_scheme(s)))
        .collect();
    for name in ["words", "reverse"] {
        let live = scheme_of(&sh, name).unwrap_or_else(|| {
            panic!("prelude binding {name:?} must be bound on the live scope and carry a scheme")
        });
        assert_eq!(
            fmt_scheme(&live),
            baked[name],
            "live scope scheme for {name:?} must match the baked entry"
        );
    }
}

// ─── (9) a session scheme instantiates polymorphically ──────────────────────

/// The scheme that crosses the run boundary is generalized against an
/// empty environment, so a later run may instantiate it at two unrelated
/// types at once — and each instantiation must keep its own roots, so the
/// `Int` use stays `Int` and the `String` use never becomes one.
#[test]
fn session_scheme_instantiates_at_two_types() {
    let mut sh = shell();
    run(&mut sh, "let idf = { |x| return $x }").unwrap();
    let stored = fmt_scheme(&scheme_of(&sh, "idf").expect("idf must carry a scheme"));
    assert!(
        stored.starts_with('∀'),
        "the stored scheme must be quantified, got: {stored}"
    );
    for (src, why) in [
        (
            "let nn = !{idf 1}\nlet ss = !{idf hello}\nreturn ()",
            "two instantiations in one run must not clash",
        ),
        (
            "let nn = !{idf 1}\nreturn $[$nn + 1]",
            "the Int must flow through the instantiation",
        ),
    ] {
        let errs = check_errors(&sh, src);
        assert!(
            errs.is_empty(),
            "{why}, got: {:?}",
            errs.iter()
                .map(|e| e.kind.render_message())
                .collect::<Vec<_>>()
        );
    }
    assert!(
        check_errors(&sh, "let ss = !{idf hello}\nreturn $[$ss + 1]")
            .iter()
            .any(|e| e.kind.code() == "T0074"),
        "the String instantiation must not admit Int arithmetic"
    );
}

/// A recursive binding is installed without a scheme, so a later run must
/// fall back to a fresh type rather than to a wrong stored one.
#[test]
fn recursive_binding_is_usable_next_run() {
    let mut sh = shell();
    run(
        &mut sh,
        "let recur = { |n| if $[$n == 0] { return 0 } else { return !{recur $[$n - 1]} } }",
    )
    .unwrap();
    let errs = check_errors(&sh, "return !{recur 3}");
    assert!(
        errs.is_empty(),
        "a recursive binding must stay usable across the run boundary, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (10) a scheme-less `set_var` binding is bound, not dropped ─────────────

/// `set_var` installs a binding with no scheme at all — the untyped sibling
/// of `bind_value`, the way a host seeds `RAL_PROMPT` and other raw vars.
/// Seeded into the next check at a fresh monomorphic variable rather than
/// dropped, the name still resolves at a command head through the ordinary
/// binding path (`exec_comp_ty`'s first arm), so it can take a lambda
/// argument no external command could — proof the call is a value
/// application, not argv rendering of an unresolved external name.
#[test]
fn set_var_block_is_bound_and_usable_as_a_command_head() {
    let mut sh = shell();
    run(&mut sh, "let sv_source = { |x| return $x }").unwrap();
    let block = sh
        .scope_lookup("sv_source")
        .cloned()
        .expect("sv_source must be bound");
    sh.set_var("sv_head".into(), block);
    assert!(
        scheme_of(&sh, "sv_head").is_none(),
        "set_var must install no scheme"
    );
    let errs = check_errors(&sh, "sv_head $sv_source");
    assert!(
        errs.is_empty(),
        "expected a scheme-less binding to resolve at a command head, taking \
         a lambda argument no external command could, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (11) a stored weak residual is one type per unit, and never aliased ────

/// What an earlier unit stores for a name of type `[_α]`, `α` being the
/// number that unit's unifier gave it.
fn stored_weak_list(alpha: u32) -> ral_core::typecheck::Scheme {
    let elem = ral_core::typecheck::TyVar(alpha);
    ral_core::test_access::scheme_with_weak_residuals(
        vec![elem],
        ral_core::typecheck::Ty::List(Box::new(ral_core::typecheck::Ty::Var(elem))),
    )
}

fn check_with_stored(names: &[&str], src: &str) -> Vec<TypeError> {
    let stored = names
        .iter()
        .map(|name| ((*name).to_string(), stored_weak_list(0)))
        .collect();
    let schemes = ral_core::test_access::with_stored_schemes(shell().session_schemes(), stored);
    match compile_and_typecheck(src, schemes, FileId::DUMMY, "", None) {
        Ok(_) => Vec::new(),
        Err(CompileError::Parse(e)) => panic!("parse: {src:?}: {e}"),
        Err(CompileError::Types(errs)) => errs,
    }
}

const ELEMENT_AS_INT_THEN_BOOL: &str = "
let [a] = $xs
let n = $[$a + 1]
let [b] = $ys
let m = $[not $b]";

/// Two names stored by different units carry the same residual number.  Each
/// is re-seeded as its own fresh weak variable, so one is used as a number
/// and the other as a Bool.
#[test]
fn stored_residuals_do_not_alias_across_names() {
    let errs = check_with_stored(&["xs", "ys"], ELEMENT_AS_INT_THEN_BOOL);
    assert!(
        errs.is_empty(),
        "residuals of two units must stay apart, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

/// One name's residual is one type for the unit that reads it twice.
#[test]
fn a_stored_residual_has_one_type_per_unit() {
    let errs = check_with_stored(&["xs"], &ELEMENT_AS_INT_THEN_BOOL.replace("$ys", "$xs"));
    assert!(
        errs.iter()
            .any(|e| e.hint().is_some_and(|h| h.contains("another use fixed it"))),
        "a weak residual must not be instantiated at a number and at a Bool, and the \
         refusal says why: {errs:?}"
    );
}

/// The contrast: a name the defining unit generalised is used at both.
#[test]
fn a_generalised_binding_is_used_at_two_types() {
    let mut sh = shell();
    run(&mut sh, "let xs = []").unwrap();
    let errs = check_errors(&sh, &ELEMENT_AS_INT_THEN_BOOL.replace("$ys", "$xs"));
    assert!(
        errs.is_empty(),
        "a generalised binding is polymorphic across runs, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── kinds ──────────────────────────────────────────────────────────────────

/// A kind survives the run boundary: the stored scheme prints it, and the
/// next run's instantiation refuses what it excludes.
#[test]
fn a_stored_scheme_keeps_its_kinds() {
    let mut sh = shell();
    run(&mut sh, "let log = { |n| echo \"n: $n\" }").unwrap();
    let stored = fmt_scheme(&scheme_of(&sh, "log").expect("log must carry a scheme"));
    assert!(stored.starts_with("∀α:scalar."), "got: {stored}");
    assert!(check_errors(&sh, "log 5\nlog text").is_empty());
    let errs = check_errors(&sh, "log [1, 2]");
    assert_eq!(
        errs.iter().map(|e| e.kind.code()).collect::<Vec<_>>(),
        ["T0074"]
    );
}

/// What a bare-label read needs is decided by the kind the target carries,
/// not by which statement came first.
#[test]
fn a_label_read_is_settled_by_a_kind_in_either_order() {
    let mut sh = shell();
    run(&mut sh, "let before = { |m| length $m; $m[a] }").unwrap();
    run(&mut sh, "let after = { |m| $m[a]; length $m }").unwrap();
    let shown = |name: &str| fmt_scheme(&scheme_of(&sh, name).expect("a scheme"));
    assert_eq!(shown("before"), "∀α. Map α → Returns α");
    assert_eq!(shown("after"), "∀α. Map α → Returns Integer");
}

// ─── checked boundaries ─────────────────────────────────────────────────────

/// The refusal a run ended in, as text.
fn refusal(result: Settled<Value>) -> String {
    match result {
        Err(ral_core::types::Break::Error(e)) => e.message,
        other => panic!("the run must be refused, got {other:?}"),
    }
}

const DECODE_A: &str = r#"let m = !{to-string '{"a": "x"}' | from-json}"#;

/// A later line's use of data decoded on an earlier one is admitted before the
/// line runs: the refusal comes ahead of its first statement.
#[test]
fn a_later_line_that_misuses_decoded_data_is_refused_before_it_runs() {
    let mut sh = shell();
    run(&mut sh, DECODE_A).unwrap();
    assert!(check_errors(&sh, "let marker = 1\nlet n = $[$m[a] + 1]").is_empty());
    let message = refusal(run(&mut sh, "let marker = 1\nlet n = $[$m[a] + 1]"));
    assert!(
        message.starts_with("$m: the value at `/a` is text (`'x'`)"),
        "{message}"
    );
    assert!(
        scheme_of(&sh, "marker").is_none(),
        "the line's first statement must not have run"
    );
}

/// What the defining unit's own uses fixed stays fixed: a residual stays weak
/// across units, so a later line cannot use the name at another type.
#[test]
fn a_later_line_cannot_use_decoded_data_at_a_type_its_unit_did_not() {
    let mut sh = shell();
    run(
        &mut sh,
        "let m = !{to-string '{\"a\": 1}' | from-json}\nlet n = $[$m[a] + 1]",
    )
    .unwrap();
    assert!(check_errors(&sh, "let k = $[$m[a] * 2]").is_empty());
    assert!(!check_errors(&sh, "let s = upper $m[a]").is_empty());
}

/// A session function that returns decoded data keeps the sites in its own IR:
/// each later line checks statically, and the function's `Fixings` make the
/// second call agree with the first on what a compared pair is (cost 11).
#[test]
fn a_session_function_returning_decoded_data_is_checked_by_its_own_site() {
    let mut sh = shell();
    run(
        &mut sh,
        "let f = { |s| let d = !{to-string $s | from-json}; let _ = $[$d[a] < $d[c]]; return $d }",
    )
    .unwrap();
    let first = r#"let x = f '{"a": 1, "c": 2}'"#;
    let second = r#"let y = f '{"a": "p", "c": "q"}'"#;
    assert!(check_errors(&sh, first).is_empty());
    assert!(check_errors(&sh, second).is_empty());
    run(&mut sh, first).unwrap();
    let message = refusal(run(&mut sh, second));
    assert!(message.contains("is a number and"), "{message}");
    assert!(
        message.contains("is text, and this script compares them"),
        "{message}"
    );
}

/// A document its own unit fixed to a cycle — json-get's walk is `μα. Map α` —
/// keeps the cycle across units and has no residual left, so the later unit
/// uses it at that type and re-admits nothing: the site that decoded it already
/// held the value to exactly this type.
#[test]
fn a_recursive_document_its_unit_fixed_is_used_later_as_fixed() {
    let mut sh = shell();
    run(
        &mut sh,
        "let doc = !{to-string '{\"a\": {\"b\": {}}}' | from-json}\n\
         let first = fold { |node key| return $node[$key] } $doc ['a']",
    )
    .unwrap();
    let stored = fmt_scheme(&scheme_of(&sh, "doc").expect("doc must carry a scheme"));
    assert!(stored.starts_with("μ"), "{stored}");
    let later = "let second = fold { |node key| return $node[$key] } $doc ['a', 'b']";
    assert!(check_errors(&sh, later).is_empty());
    let top = compile_and_typecheck(later, sh.session_schemes(), FileId::DUMMY, "", None)
        .expect("the later line checks");
    assert!(top.admits.is_empty(), "nothing is left to admit again");
    run(&mut sh, later).unwrap();
}

/// What a unit leaves undecided is admitted again by each unit that uses it,
/// once, before it runs.
#[test]
fn a_residual_datum_is_admitted_again_by_each_unit_that_uses_it() {
    let mut sh = shell();
    run(&mut sh, DECODE_A).unwrap();
    let admits = |src: &str| {
        compile_and_typecheck(src, sh.session_schemes(), FileId::DUMMY, "", None)
            .expect("the line checks")
            .admits
            .len()
    };
    assert_eq!(admits("let k = length $m"), 1);
    assert_eq!(admits("let k = length $m\nlet j = length $m"), 1);
    assert_eq!(admits("let k = 1"), 0);
}
