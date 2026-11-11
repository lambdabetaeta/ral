//! What a top-level run leaves in the session across calls — the one thing an
//! in-script golden cannot observe — with and without an active sandbox
//! projection, plus the one `within [dir:]` exit the script that ran it cannot
//! outlive, the environment overrides a `Shell` carries, and the `try` record
//! and `$STATUS` name seen from the run entry.  Block scoping, `cd` in blocks
//! and the working-directory cell's handler are goldens: `tests/run/lang/scoping.ral`
//! and `tests/run/unix/cwd-blocks.ral`.
//!
//! The two-call harness mirrors what exarch's `shell_eval::run_shell`
//! does between consecutive tool calls and what the ral REPL's
//! `execute_input` does between consecutive prompt runs: hold a single
//! [`Shell`] across calls and route each body through the public
//! `run` entry. Exarch's root holds its grants as session layers, so this
//! harness installs each ceiling once.
//!
//! The ADR session-scheme-continuity Verify list is exercised end-to-end
//! below: each run's static check is seeded from the live session, the schemes
//! the checker inferred for run *N*'s top-level binds living on the runtime
//! bindings, so run *N+1*'s check sees them. Installed handler names inform
//! diagnostics, not command types. Its harness mirrors the REPL loop in
//! `ral/src/repl/exec.rs`: `check_errors` seeds `compile_and_typecheck` from
//! the live `session_schemes()`, and `top_level` drives the public `run` entry.

use crate::common::{must_succeed, prelude_shell, run_on, top_level};

use ral_core::authority::policy::Capabilities;
#[cfg(unix)]
use ral_core::authority::policy::FsPolicy;
use ral_core::compile::{CompileError, compile_and_typecheck};
use ral_core::ir::Phrase;
use ral_core::path::shown;
use ral_core::protocol::Run;
use ral_core::shell::boot::HostSurface;
use ral_core::shell::run::RunReport;
use ral_core::shell::runtime::{Mooring, Settled};
use ral_core::source::{FileId, SourceOrigin};
use ral_core::syntax::quote_word;
use ral_core::test_access::{binding_scheme as scheme_of, has_weak_residuals};
use ral_core::test_helper::{fresh_shell, layer};
use ral_core::{Break, SessionSchemes, Shell, TypeError, Value};
use std::path::{Path, PathBuf};

// ── Harness ─────────────────────────────────────────────────────────────

fn shell_under(caps: Capabilities) -> Shell {
    let mut shell = fresh_shell();
    shell.push_session_capabilities(layer(caps));
    shell
}

/// Capability frame that actually triggers `sandbox_projection()` to
/// return `Some(_)`: any `fs` policy is enough.  Read prefix `/` makes
/// every read pass the in-process guard; the projection still goes through
/// the OS-sandbox machinery because `saw_fs` is true.
#[cfg(unix)]
fn projecting_caps() -> Capabilities {
    Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![ral_core::path::FrozenPath::root()],
            write_prefixes: vec![ral_core::path::FrozenPath::root()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    }
}

/// The frames under which top-level state semantics must agree: an active
/// projection changes where children are confined, never what a run leaves in
/// the session.
fn parity_caps() -> Vec<Capabilities> {
    #[cfg(unix)]
    return vec![Capabilities::default(), projecting_caps()];
    #[cfg(not(unix))]
    return vec![Capabilities::default()];
}

/// `path` as ral shows it (`/`-separated on every host, as `cwd` returns it),
/// without a trailing separator: `std::env::temp_dir()` may end in one
/// (`/tmp/`, `…\Temp\`), and `Shell::cwd()` — canonicalised — never does.
/// Re-collecting the components drops it and keeps the root itself.
fn shown_dir(path: &Path) -> String {
    shown(path.components().collect::<PathBuf>())
}

// ── (1) Top-level persistence ───────────────────────────────────────────

/// Two sequential top-level calls share state: a `let` from the first
/// call is visible in the second.  This is the load-bearing property of
/// the run's install-mobile-on-Ok rule, and the whole reason exarch's
/// tool-call harness routes through the top-level run entry instead of
/// the block boundary.
#[test]
fn top_level_let_persists_across_calls() {
    for caps in parity_caps() {
        let mut shell = shell_under(caps);
        top_level(&mut shell, "let persist_n = 41").expect("first run");
        let result = top_level(&mut shell, "return $[$persist_n + 1]").expect("second run");
        assert_eq!(result, Value::Int(42));
    }
}

// ── (2) Top-level partial effects ───────────────────────────────────────

/// A top-level run that fails partway still installs the mobile mutations
/// made before the failure.  This locks in the run's install-on-Error rule:
/// bindings made before a fatal command survive into the next run; bindings
/// *after* the failure do not exist because that line never ran.
#[test]
fn top_level_partial_effects_persist_on_error() {
    for caps in parity_caps() {
        let mut shell = shell_under(caps);
        let _ = top_level(
            &mut shell,
            "let pre_fail_x = 1\nfail x\nlet post_fail_y = 2",
        );
        assert!(
            shell.scope_lookup("pre_fail_x").is_some(),
            "pre-failure `let` must survive into the next run"
        );
        assert!(
            shell.scope_lookup("post_fail_y").is_none(),
            "post-failure `let` never ran, must not be present"
        );
    }
}

/// A destructuring bind is all-or-nothing even across the run entry.  The
/// outer pattern's first element matches and would stage `matched_x`; the
/// second fails.  Since a top-level run installs its mobile on error (the
/// sibling test above), a stage-as-you-recurse regression would leak
/// `matched_x` into the session — a half-destructured record visible at the
/// next prompt.
#[test]
fn top_level_partial_destructure_binds_nothing() {
    let mut shell = fresh_shell();
    let result = top_level(
        &mut shell,
        "let [[matched_x], [unmatched_a, unmatched_b]] = [[1], [2]]",
    );
    assert!(result.is_err(), "the inner pattern must fail to match");
    for name in ["matched_x", "unmatched_a", "unmatched_b"] {
        assert!(
            shell.scope_lookup(name).is_none(),
            "`{name}` must not survive a partially matched destructure"
        );
    }
}

// ── (3) Top-level cwd ───────────────────────────────────────────────────

/// `cd` in one top-level run is visible to subsequent runs: a later
/// `cwd` reflects the directory set by the earlier `cd`, the session
/// being the cell's outermost handler.
#[test]
fn top_level_cd_persists_across_calls() {
    for caps in parity_caps() {
        let mut shell = shell_under(caps);
        let tmp = shown_dir(ral_core::test_helper::real(std::env::temp_dir()).as_path());
        top_level(&mut shell, &format!("cd '{tmp}'")).expect("cd should succeed");
        let result = top_level(&mut shell, "cwd").expect("cwd should succeed");
        assert_eq!(result, Value::string(tmp), "cwd after cd");
    }
}

// ── (4) `within [dir:]` and `exit` ──────────────────────────────────────

/// An `exit` inside `within [dir:]` ends the run, and the session's cell is
/// still restored: a script cannot observe its own cwd after it has exited.
#[test]
fn within_dir_restores_the_cell_on_exit() {
    let root = tempfile::tempdir().unwrap();
    let outer = root.path().join("outer");
    let inner = root.path().join("inner");
    std::fs::create_dir(&outer).unwrap();
    std::fs::create_dir_all(inner.join("sub")).unwrap();
    let mut shell = fresh_shell();
    top_level(&mut shell, &format!("cd '{}'", outer.display())).expect("cd");
    // `cd` resolves host aliases; restoration must preserve that exact cell.
    let before = shell.cwd();
    let exited = top_level(
        &mut shell,
        &format!("within [dir: '{}'] {{ cd sub; exit 0 }}", inner.display()),
    );
    assert!(matches!(exited, Err(Break::Escape(_))), "got {exited:?}");
    assert_eq!(shell.cwd(), before, "an exit must restore the cell");
}

// ── (5) Environment and names ───────────────────────────────────────────

#[test]
fn try_error_record_carries_the_script() {
    assert_eq!(
        must_succeed(
            "try { fail 'x' } { |err| \
             return \"$err[site][script]:$err[site][line]\" }"
        ),
        Value::string("<test>:1")
    );
}

#[test]
fn env_overrides_shadow_process_env_in_dollar_env() {
    let mut shell = prelude_shell();
    shell.set_env_var("RAL_TEST_ENV", "override").unwrap();
    let result = run_on(&mut shell, "return !{env}[RAL_TEST_ENV]").expect("evaluate env");
    assert_eq!(result, Value::string("override"));
}

/// `$STATUS` is not a name: `try` binds a record carrying the failure reason.
#[test]
fn status_is_an_unbound_variable_that_names_its_replacement() {
    let mut shell = prelude_shell();
    let RunReport::Static { diagnostics } = shell.run(Run::foreground(
        "return $STATUS",
        SourceOrigin::named("<test>"),
    )) else {
        panic!("$STATUS must be rejected statically");
    };
    let rendered = diagnostics.render(shell.exit_hints()).0;
    assert!(
        rendered.contains("undefined variable: $STATUS"),
        "unexpected diagnostic: {rendered}"
    );
    assert!(
        rendered.contains("`try`"),
        "the diagnostic must point at `try`: {rendered}"
    );
}

/// A session `let` shadows the command of the same name, in every binder:
/// plain, recursive, and destructuring.
#[cfg(unix)]
#[test]
fn session_scope_let_shadows_a_path_command() {
    assert_eq!(must_succeed("let cat = 1\nreturn $cat"), Value::Int(1));
    assert_eq!(
        must_succeed(
            "let cat = { |x| if $[$x <= 0] { return 0 } else { cat $[$x - 1] } }\nreturn !{cat 3}"
        ),
        Value::Int(0)
    );
    assert_eq!(
        must_succeed("let [cat, other] = [1, 2]\nreturn $[$cat + $other]"),
        Value::Int(3)
    );
}

// ── The session-scheme-continuity Verify list ───────────────────────────

fn shell() -> Shell {
    prelude_shell()
}

/// Check `src` against the live session without evaluating it — the
/// errors a run would surface before running.
fn check_errors(shell: &Shell, src: &str) -> Vec<TypeError> {
    match compile_and_typecheck(src, shell.session_schemes(), FileId::DUMMY, None, None) {
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
    top_level(&mut sh, "let f = { return 3 }").unwrap();
    let errs = check_errors(&sh, "f | from-json");
    assert_eq!(
        errs.iter().map(|e| e.kind.code()).collect::<Vec<_>>(),
        ["T0011"]
    );
}

// ─── (2) byte producer into byte consumer typechecks via harvested scheme ────

/// `let f = { echo hi }` then `f | wc -l`: the harvested scheme for `f`
/// is `Command`, so it may feed `wc`'s input.
#[test]
fn byte_producer_into_byte_consumer_typechecks() {
    let mut sh = shell();
    top_level(&mut sh, "let f = { echo hi }").unwrap();
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
/// reported statically: `xv` is `String` (the literal `'hello'`), so
/// `$xv + 1` requires a number where the stored scheme supplies a String.
#[test]
fn cross_run_value_type_error_is_static() {
    let mut sh = shell();
    top_level(&mut sh, "let xv = 'hello'").unwrap();
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
    top_level(&mut sh, "let xv = 3").unwrap();
    top_level(&mut sh, "let xv = 'hello'").unwrap();
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
    top_level(&mut sh, "let xv = 1").unwrap();
    // The bad command aborts the run before the rebind runs.
    let outcome = top_level(&mut sh, "nonexistent-command-zz\nlet xv = hello");
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
    top_level(&mut sh, "let [a, bb] = [1, 2]").unwrap();
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
    top_level(&mut sh, "let cnt = 3").unwrap();
    assert!(
        scheme_of(&sh, "cnt").is_some(),
        "a plain let binding must carry a scheme"
    );
}

// ─── (7) handler visibility ────────────────────────────────────────────────────

/// A host-installed handler's name informs the next run's diagnostics, not
/// its command type: `three` still takes only text. Removing its frame makes
/// `$three` unbound rather than a non-first-class handler entry.
#[test]
fn handler_visible_to_next_run() {
    let mut sh = shell();
    let arm = top_level(&mut sh, "return { |args| echo 3 }").unwrap();
    let frame = sh.install_handler("three".into(), arm).unwrap();
    assert!(
        check_errors(&sh, "three").is_empty(),
        "expected the handler used alone to be clean, got: {:?}",
        check_errors(&sh, "three")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    let errs = check_errors(&sh, "return $three");
    assert!(
        errs.iter()
            .any(|e| e.kind.render_message().contains("handler entry")),
        "expected the installed handler name to reject `$three` as a handler entry, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    let errs = check_errors(&sh, "three [1, 2]");
    assert!(
        errs.iter().filter_map(TypeError::hint).any(|hint| {
            hint.contains("`three` has a handler here") && hint.contains("takes only text")
        }),
        "a handled external command must retain the text-only gate: {errs:?}"
    );
    assert!(sh.remove_handler_frame(frame));
    let codes: Vec<_> = check_errors(&sh, "return $three")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(
        codes,
        ["T0071"],
        "after removing the frame `$three` is merely unbound, not a handler entry"
    );
}

// ─── (9) a session scheme instantiates polymorphically ──────────────────────

/// The scheme that crosses the run boundary is generalized against an
/// empty environment, so a later run may instantiate it at two unrelated
/// types at once — and each instantiation must keep its own roots, so the
/// `Int` use stays `Int` and the `String` use never becomes one.
#[test]
fn session_scheme_instantiates_at_two_types() {
    let mut sh = shell();
    top_level(&mut sh, "let idf = { |x| return $x }").unwrap();
    let stored = scheme_of(&sh, "idf")
        .expect("idf must carry a scheme")
        .to_string();
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
    top_level(
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
    top_level(&mut sh, "let sv_source = { |x| return $x }").unwrap();
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

#[test]
fn settled_weak_block_instantiates_at_both_grades_next_unit() {
    let surface = HostSurface::default();
    let first = compile_and_typecheck(
        "let f = from-json; each $f [1]",
        crate::common::schemes_for(&surface),
        FileId::DUMMY,
        None,
        None,
    )
    .unwrap();
    let Phrase::Define { schemes, .. } = &first.phrases[0].item else {
        panic!("expected a Define for f");
    };
    assert!(schemes.iter().any(|(name, _)| name == "f"));
    compile_and_typecheck(
        "if true { f 1 } else { echo a }; let y = f 1",
        SessionSchemes::from_prelude(&first, surface.manifest()),
        FileId::DUMMY,
        None,
        None,
    )
    .unwrap();
}

#[test]
fn scheme_less_host_binding_exports_a_closed_weak_scheme() {
    let mut sh = shell();
    sh.set_var("sv".into(), Value::Int(1));
    let checked = compile_and_typecheck(
        "let y = $sv",
        sh.session_schemes(),
        FileId::DUMMY,
        None,
        None,
    )
    .unwrap();
    let Phrase::Define { schemes, .. } = &checked.phrases[0].item else {
        panic!("expected a Define for y");
    };
    let (_, scheme) = schemes.iter().find(|(name, _)| name == "y").unwrap();
    assert!(has_weak_residuals(scheme));
    assert_eq!(scheme.to_string(), "_α");
}

#[test]
fn host_fold_wrapper_exports_a_closed_grade_polymorphic_scheme() {
    let mut sh = shell();
    sh.set_var("sv".into(), Value::Int(1));
    let first = compile_and_typecheck(
        "let take_step = { |f| fold $f () [()] }\n\
         take_step $sv\n\
         let y = { sv () () }",
        sh.session_schemes(),
        FileId::DUMMY,
        None,
        None,
    )
    .unwrap();
    let Phrase::Define { schemes, .. } = &first.phrases[2].item else {
        panic!("expected a Define for y");
    };
    let (_, scheme) = schemes.iter().find(|(name, _)| name == "y").unwrap();
    assert!(!has_weak_residuals(scheme));
    // The quantified grade can still become a command (SPEC §17.3).
    assert_eq!(scheme.to_string(), "∀ν. Command[ν] Unit");

    let next = compile_and_typecheck(
        "if true { y } else { echo a }; let result = y",
        SessionSchemes::from_prelude(&first, HostSurface::default().manifest()),
        FileId::DUMMY,
        None,
        None,
    )
    .unwrap();
    let Phrase::Define { schemes, .. } = &next.phrases[1].item else {
        panic!("expected a Define for result");
    };
    let (_, scheme) = schemes.iter().find(|(name, _)| name == "result").unwrap();
    assert_eq!(scheme.to_string(), "Unit");
}

#[test]
fn host_fold_wrapper_keeps_a_grade_fixed_after_generalization_next_unit() {
    let mut sh = shell();
    sh.set_var("sv".into(), Value::Int(1));
    sh.set_var("sv_data".into(), Value::Int(1));
    let first = compile_and_typecheck(
        "let take_step = { |f| fold $f () [()] }\n\
         take_step $sv\n\
         let y = [run: { sv () () }, data: $sv_data]\n\
         if true { sv () () } else { echo a }",
        sh.session_schemes(),
        FileId::DUMMY,
        None,
        None,
    )
    .unwrap();
    let Phrase::Define { schemes, .. } = &first.phrases[2].item else {
        panic!("expected a Define for y");
    };
    let (_, scheme) = schemes.iter().find(|(name, _)| name == "y").unwrap();
    assert!(has_weak_residuals(scheme));
    assert!(scheme.to_string().contains("run: {Command}"));
    assert!(!scheme.to_string().starts_with('∀'));

    let next = compile_and_typecheck(
        "let run = $y[run]; let captured = run; upper $captured",
        SessionSchemes::from_prelude(&first, HostSurface::default().manifest()),
        FileId::DUMMY,
        None,
        None,
    )
    .unwrap();
    let Phrase::Define { schemes, .. } = &next.phrases[1].item else {
        panic!("expected a Define for captured");
    };
    let (_, scheme) = schemes.iter().find(|(name, _)| name == "captured").unwrap();
    assert_eq!(scheme.to_string(), "String");
}

// ─── a generalised binding stays polymorphic across runs ───────────────────

const ELEMENT_AS_INT_THEN_BOOL: &str = "
let [a] = $xs
let n = $[$a + 1]
let [b] = $ys
let m = $[not $b]";

/// A name the defining unit generalised is used at both types.
#[test]
fn a_generalised_binding_is_used_at_two_types() {
    let mut sh = shell();
    top_level(&mut sh, "let xs = []").unwrap();
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
    top_level(&mut sh, "let log = { |n| echo \"n: $n\" }").unwrap();
    let stored = scheme_of(&sh, "log")
        .expect("log must carry a scheme")
        .to_string();
    assert!(stored.starts_with("∀α:scalar."), "got: {stored}");
    assert!(check_errors(&sh, "log 5\nlog text").is_empty());
    let errs = check_errors(&sh, "log [1, 2]");
    assert_eq!(
        errs.iter().map(|e| e.kind.code()).collect::<Vec<_>>(),
        ["T0074"]
    );
}

#[test]
fn a_sized_kind_does_not_change_the_key_forms_read_rule() {
    let mut sh = shell();
    for src in [
        "let before = { |m| length $m; $m[a] }",
        "let after = { |m| $m[a]; length $m }",
    ] {
        let errs = check_errors(&sh, src);
        assert_eq!(
            errs.iter().map(|e| e.kind.code()).collect::<Vec<_>>(),
            ["T0074"],
            "{src}: {errs:?}"
        );
        assert!(
            errs[0].kind.render_message().contains("a record"),
            "{src}: {errs:?}"
        );
    }
    top_level(&mut sh, "let before = { |m k| length $m; $m[$k] }").unwrap();
    top_level(&mut sh, "let after = { |m k| $m[$k]; length $m }").unwrap();
    let shown = |name: &str| scheme_of(&sh, name).expect("a scheme").to_string();
    // The generic read's fresh grade is visible (SPEC §17.3),
    // unlike the grade paired with `length`'s concrete Integer below.
    assert_eq!(shown("before"), "∀α ν. Map α → String → Command[ν] α");
    assert_eq!(shown("after"), "∀α. Map α → String → Returns Integer");
}

// ─── checked boundaries ─────────────────────────────────────────────────────

/// The refusal a run ended in, as text.
fn refusal(result: Settled<Value>) -> String {
    match result {
        Err(ral_core::shell::runtime::Break::Error(e)) => e.describe().into_owned(),
        other => panic!("the run must be refused, got {other:?}"),
    }
}

const WEAK_DECODER: &str = "let decode = { |s| to-string $s | from-json }";

#[test]
fn imported_weak_functions_protect_every_higher_order_route() {
    let cases = [
        ("", "let n = decode '\"bad\"'; let result = $[$n + 1]"),
        (
            "let apply = { |f x| f $x }",
            "let n = apply $decode '\"bad\"'; let result = $[$n + 1]",
        ),
        (
            "let get = { return $decode }",
            "let f = get; let n = f '\"bad\"'; let result = $[$n + 1]",
        ),
        (
            "let box = [f: $decode]",
            "let n = !{$box[f] '\"bad\"'}; let result = $[$n + 1]",
        ),
        (
            "let decode2 = { |ignored s| decode $s }",
            "let partial = decode2 () ; let n = partial '\"bad\"'; let result = $[$n + 1]",
        ),
    ];
    for (setup, later) in cases {
        let mut sh = shell();
        top_level(&mut sh, &format!("{WEAK_DECODER}\n{setup}")).unwrap();
        assert!(check_errors(&sh, later).is_empty(), "{later}");
        let message = refusal(top_level(&mut sh, later));
        assert!(message.contains("imported value"), "{later}: {message}");
        assert!(message.contains("the string \"bad\""), "{message}");
    }
}

#[test]
fn imported_weak_occurrences_keep_their_own_quantified_instances() {
    let mut sh = shell();
    top_level(
        &mut sh,
        "let f = { |x| let d = !{to-string '\"ok\"' | from-json}; return [item: $x, data: $d] }",
    )
    .unwrap();
    top_level(
        &mut sh,
        "let a = f true; let b = f 1; let n = $[$b[item] + 1]",
    )
    .unwrap();
}

#[test]
fn imported_comparable_parameters_stay_polymorphic_inside_closures() {
    let mut sh = shell();
    top_level(
        &mut sh,
        "let f = { |x| let d = !{to-string '0' | from-json}; return [less: $[$x < $x], data: $d] }",
    )
    .unwrap();
    top_level(
        &mut sh,
        "let g = { |x| f $x }; let a = g 1; let b = g hello",
    )
    .unwrap();
}

#[test]
fn returned_protected_decoders_retain_their_contract_when_host_bound() {
    let mut sh = shell();
    top_level(&mut sh, WEAK_DECODER).unwrap();
    let decoder = top_level(&mut sh, "return $decode").unwrap();
    sh.bind_value("host_decode".into(), decoder).unwrap();
    let scheme = scheme_of(&sh, "host_decode")
        .expect("a re-inferred scheme")
        .to_string();
    assert!(scheme.contains("→ Returns _"), "{scheme}");
    let later = "let n = host_decode '\"bad\"'; let result = $[$n + 1]";
    assert!(check_errors(&sh, later).is_empty());
    let message = refusal(top_level(&mut sh, later));
    assert!(message.contains("imported value"), "{message}");
}

#[test]
fn imported_weak_callbacks_are_protected_at_the_argument() {
    let mut sh = shell();
    top_level(
        &mut sh,
        "let invoke = { |f| let x = !{to-string '\"bad\"' | from-json}; f $x }",
    )
    .unwrap();
    let later = "invoke { |x| return $[$x + 1] }";
    assert!(check_errors(&sh, later).is_empty());
    let message = refusal(top_level(&mut sh, later));
    assert!(message.contains("imported value"), "{message}");
}

#[test]
fn imported_weak_handles_protect_every_payload_observation() {
    let cases = [
        "let n = await $h; let result = $[$n + 1]",
        "let n = race [$h]; let result = $[$n + 1]",
        "let n = case !{poll $h} [\
            `running: { |_| return 0 }, `stopping: { |_| return 0 },\
            `consumed: { |_| return 0 },\
            `ready: { |d| case $d[outcome] [\
                `ok: { |n| return $[$n + 1] }, `err: { |_| return 0 }\
            ] }\
        ]",
    ];
    for later in cases {
        let mut sh = shell();
        top_level(
            &mut sh,
            "let h = spawn { to-string '\"bad\"' | from-json }; let _ = first-settled [$h]",
        )
        .unwrap();
        assert!(check_errors(&sh, later).is_empty(), "{later}");
        let message = refusal(top_level(&mut sh, later));
        assert!(message.contains("imported value"), "{later}: {message}");
    }
}

#[test]
fn imported_handle_views_share_claims_without_claiming_at_wrap() {
    let mut sh = shell();
    top_level(
        &mut sh,
        "let h = spawn { to-string '1' | from-json }; let _ = first-settled [$h]",
    )
    .unwrap();
    top_level(&mut sh, "let alias = $h").unwrap();
    top_level(&mut sh, "let n = await $alias; let result = $[$n + 1]").unwrap();
    let message = refusal(top_level(&mut sh, "await $h"));
    assert!(message.contains("already claimed"), "{message}");
}

#[test]
fn imported_weak_handle_payloads_keep_delayed_obligations() {
    let mut sh = shell();
    top_level(&mut sh, "let h = spawn { return [f: { |s| to-string $s | from-json }] }; let _ = first-settled [$h]").unwrap();
    let later = "let box = await $h; let n = $box[f] '\"bad\"'; let result = $[$n + 1]";
    assert!(check_errors(&sh, later).is_empty());
    let message = refusal(top_level(&mut sh, later));
    assert!(message.contains("imported value"), "{message}");
}

#[test]
fn imported_weak_protection_survives_a_third_run() {
    let mut sh = shell();
    top_level(&mut sh, WEAK_DECODER).unwrap();
    top_level(&mut sh, "let next = [f: $decode]").unwrap();
    let later = "let n = !{$next[f] '\"bad\"'}; let result = $[$n + 1]";
    assert!(check_errors(&sh, later).is_empty());
    let message = refusal(top_level(&mut sh, later));
    assert!(message.contains("imported value"), "{message}");
}

const DECODE_A: &str = r#"let m = !{to-string '{"a": "x"}' | from-json}"#;

/// A later line's use of data decoded on an earlier one is admitted before the
/// line runs: the refusal comes ahead of its first statement.
#[test]
fn a_later_line_that_misuses_decoded_data_is_refused_before_it_runs() {
    let mut sh = shell();
    top_level(&mut sh, DECODE_A).unwrap();
    assert!(check_errors(&sh, "let marker = 1\nlet n = $[$m[a] + 1]").is_empty());
    let message = refusal(top_level(&mut sh, "let marker = 1\nlet n = $[$m[a] + 1]"));
    assert!(
        message.starts_with("$m: the value at `/a` is the string \"x\""),
        "{message}"
    );
    assert!(
        scheme_of(&sh, "marker").is_none(),
        "the line's first statement must not have run"
    );
}

#[test]
fn a_module_readmits_session_data_before_its_first_phrase() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ral_core::test_helper::real(tmp.path());
    let module = root.as_path().join("lib.ral");
    let marker = root.as_path().join("ran");
    let source = format!(
        "to-string ran > {}\nlet y = $[$d + 1]",
        quote_word(&shown(&marker))
    );
    std::fs::write(&module, &source).unwrap();
    let mut sh = shell();
    top_level(&mut sh, "let d = !{to-string '\"s\"' | from-json}").unwrap();
    assert!(check_errors(&sh, &source).is_empty());

    let load = format!("use {}", quote_word(&shown(&module)));
    let Break::Error(error) = top_level(&mut sh, &load).expect_err("$d is not an integer") else {
        panic!("expected a module admission error");
    };
    let message = error.describe();
    assert!(
        message.contains("$d: the value is the string \"s\""),
        "{message}"
    );
    assert!(message.contains("uses it as an integer"), "{message}");
    assert_eq!(error.context(), Some("use"));
    let record = error.fact(&sh);
    assert_eq!(record.site().script.as_ref(), shown(&module));
    assert_eq!(record.site().line, 2);
    assert!(!marker.exists(), "the module's first phrase must not run");

    top_level(&mut sh, "let d = 1").unwrap();
    top_level(&mut sh, &load).expect("the module runs with integer session data");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");
}

#[test]
fn evaluate_source_readmits_session_data_before_its_first_phrase() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("ran");
    let source = format!(
        "to-string ran > {}\nlet y = $[$d + 1]",
        quote_word(&shown(&marker))
    );
    let origin = SourceOrigin::named("lib.ral");
    let mut sh = shell();
    top_level(&mut sh, "let d = !{to-string '\"s\"' | from-json}").unwrap();
    assert!(check_errors(&sh, &source).is_empty());

    let Break::Error(error) =
        ral_core::shell::load::evaluate_source(&Mooring::adrift(), &mut sh, &source, &origin, None)
            .expect_err("$d is not an integer")
    else {
        panic!("expected a source admission error");
    };
    let message = error.describe();
    assert!(
        message.contains("$d: the value is the string \"s\""),
        "{message}"
    );
    assert!(message.contains("uses it as an integer"), "{message}");
    let record = error.fact(&sh);
    assert_eq!(record.site().script.as_ref(), "lib.ral");
    assert_eq!(record.site().line, 2);
    assert!(!marker.exists(), "the source's first phrase must not run");
    assert!(sh.scope_lookup("y").is_none());
    assert_eq!(sh.scope_lookup("d"), Some(&Value::string("s")));

    top_level(&mut sh, "let d = 1").unwrap();
    ral_core::shell::load::evaluate_source(&Mooring::adrift(), &mut sh, &source, &origin, None)
        .expect("the source runs with integer session data");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");
    assert_eq!(sh.scope_lookup("y"), Some(&Value::Int(2)));
}

/// What the defining unit's own uses fixed stays fixed: a residual stays weak
/// across units, so a later line cannot use the name at another type.
#[test]
fn a_later_line_cannot_use_decoded_data_at_a_type_its_unit_did_not() {
    let mut sh = shell();
    top_level(
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
    top_level(
        &mut sh,
        "let f = { |s| let d = !{to-string $s | from-json}; let _ = $[$d[a] < $d[c]]; return $d }",
    )
    .unwrap();
    let first = r#"let x = f '{"a": 1, "c": 2}'"#;
    let second = r#"let y = f '{"a": "p", "c": "q"}'"#;
    assert!(check_errors(&sh, first).is_empty());
    assert!(check_errors(&sh, second).is_empty());
    top_level(&mut sh, first).unwrap();
    let message = refusal(top_level(&mut sh, second));
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
    top_level(
        &mut sh,
        "let doc = !{to-string '{\"a\": {\"b\": {}}}' | from-json}\n\
         let first = fold { |node key| return $node[$key] } $doc ['a']",
    )
    .unwrap();
    let stored = scheme_of(&sh, "doc")
        .expect("doc must carry a scheme")
        .to_string();
    assert!(stored.starts_with("μ"), "{stored}");
    let later = "let second = fold { |node key| return $node[$key] } $doc ['a', 'b']";
    assert!(check_errors(&sh, later).is_empty());
    let top = compile_and_typecheck(later, sh.session_schemes(), FileId::DUMMY, None, None)
        .expect("the later line checks");
    assert!(top.admits.is_empty(), "nothing is left to admit again");
    top_level(&mut sh, later).unwrap();
}

/// What a unit leaves undecided is admitted again by each unit that uses it,
/// once, before it runs.
#[test]
fn a_residual_datum_is_admitted_again_by_each_unit_that_uses_it() {
    let mut sh = shell();
    top_level(&mut sh, DECODE_A).unwrap();
    let admits = |src: &str| {
        compile_and_typecheck(src, sh.session_schemes(), FileId::DUMMY, None, None)
            .expect("the line checks")
            .admits
            .len()
    };
    assert_eq!(admits("let k = length $m"), 1);
    assert_eq!(admits("let k = length $m\nlet j = length $m"), 1);
    assert_eq!(admits("let k = 1"), 0);
}

/// An imported weak value is given a membrane only where a thunk or handle
/// hides in its contract: pure data was admitted whole, once, and costs
/// nothing per occurrence.
#[test]
fn only_imported_values_with_delayed_parts_are_given_a_membrane() {
    let mut sh = shell();
    top_level(&mut sh, "let d = !{to-string '[1,2]' | from-json}").unwrap();
    top_level(&mut sh, WEAK_DECODER).unwrap();
    let membrane = |src: &str| {
        let top = compile_and_typecheck(src, sh.session_schemes(), FileId::DUMMY, None, None)
            .expect("the line checks");
        format!("{:?}", top.phrases).contains("Protected {")
    };
    assert!(!membrane("let k = length $d"));
    assert!(membrane("let box = [f: $decode]"));
}
