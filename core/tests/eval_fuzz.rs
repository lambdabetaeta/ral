#![allow(clippy::disallowed_methods)]

//! Evaluator fuzz tests: every value operation, type interaction, control flow
//! path, and edge case must produce a defined result — never panic.

mod common;

use ral_core::builtins;
#[cfg(unix)]
use ral_core::types::{Capabilities, ExecMap, ExecPolicy};
use ral_core::{
    Break, Error, Shell, Value, elaborator::elaborate, evaluator::evaluate, syntax::parser::parse,
    typecheck,
};
#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

fn eval(input: &str) -> ral_core::types::Settled<Value> {
    let ast = parse(input).map_err(|e: ral_core::syntax::parser::ParseError| {
        Break::Error(Error::new(e.to_string(), 2))
    })?;
    let comp = elaborate(&ast, std::collections::HashSet::default());
    // The evaluator reads its mode wires off the annotated comp, so it
    // must run the checked IR, not the bare elaboration whose wires are
    // still the elaborator's placeholder.
    let comp = match typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes()),
    ) {
        Ok(annotated) => std::sync::Arc::new(annotated),
        Err(errors) => {
            let msg = errors
                .iter()
                .map(|e| e.kind.render_message())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Break::Error(Error::new(format!("type error: {msg}"), 2)));
        }
    };
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    builtins::register(&mut shell, common::prelude_comp());
    evaluate(&comp, &mut shell)
}

fn must_succeed(input: &str) -> Value {
    eval(input).unwrap_or_else(|e| panic!("should succeed: {input:?}\n  error: {e:?}"))
}

fn must_fail(input: &str) {
    assert!(eval(input).is_err(), "should fail: {input:?}");
}

fn must_not_panic(input: &str) {
    let _ = eval(input);
}

// ── Type system basics ───────────────────────────────────────────────────

#[test]
fn literal_negative_int() {
    // -3 is parsed as bare word "-3" → Int(-3)
    assert_eq!(must_succeed("return -3"), Value::Int(-3));
}

#[test]
fn literal_string_in_assignment() {
    // Assign a string literal and read it back.
    assert_eq!(
        must_succeed("let xv = 'hello'\nreturn $xv"),
        Value::String("hello".into())
    );
}

#[test]
fn quoted_literal_pipeline_stage_is_not_executed_as_command() {
    // `'abc'` is a value-producing stage (`∅` output), not a command: it
    // feeds the next stage as a value.  The external consumer `blah` has
    // an open input the value edge pins to `∅`, so the pipeline is well
    // typed — it fails only because `blah` is not a command on PATH, never
    // because `'abc'` was run as one.
    let err = match eval("'abc' | blah") {
        Err(Break::Error(err)) => err,
        other => panic!("should fail because blah is unknown: {other:?}"),
    };
    assert!(
        err.message.contains("blah") && err.message.contains("not found"),
        "unexpected error: {}",
        err.message
    );
}

// ── Arithmetic type preservation ─────────────────────────────────────────

#[test]
fn arith_division_by_zero() {
    must_fail("$[1 / 0]");
}

#[test]
fn arith_modulo_by_zero() {
    must_fail("$[1 % 0]");
}

#[test]
fn arith_div_min_by_neg_one_is_a_clean_error() {
    // i64::MIN / -1 overflows i64; Rust panics in both debug and release, and
    // the release profile aborts.  A checked division surfaces it as an error.
    // i64::MIN has no literal (it exceeds the i64 parse bound), so build it by
    // subtraction.
    must_fail("$[ ( 0 - 9223372036854775807 - 1 ) / ( 0 - 1 ) ]");
}

#[test]
fn arith_mod_min_by_neg_one_is_a_clean_error() {
    // i64::MIN % -1 overflows the same way; checked_rem turns it into an error.
    must_fail("$[ ( 0 - 9223372036854775807 - 1 ) % ( 0 - 1 ) ]");
}

// ── Strict Bool conditionals ─────────────────────────────────────────────

#[test]
fn if_with_forced_block_returning_bool() {
    must_succeed("if !{ return $[1 == 1] } { echo yes }");
}

#[test]
fn if_with_bare_block_condition_is_type_error() {
    // { return $[1 == 1] } is a thunk U(F Bool), not F Bool.
    must_fail("if { return $[1 == 1] } { echo yes }");
}

// ── if: one-armed ───────────────────────────────────────────────────────

#[test]
fn if_one_armed_true_runs_body() {
    assert_eq!(
        must_succeed("let xv = 0\nif true { let xv = 1 }\nreturn $xv"),
        // one-armed if runs for side effects; outer x is shadowed inside
        // the block but not outside — still 0
        Value::Int(0),
    );
}

#[test]
fn if_one_armed_false_skips_body() {
    must_succeed("if false { fail [status: 1] }");
}

#[test]
fn if_one_armed_returns_unit() {
    assert_eq!(must_succeed("return !{if true { echo yes }}"), Value::Unit);
}

// ── if: two-armed ──────────────────────────────────────────────────────

#[test]
fn if_two_armed_returns_then() {
    assert_eq!(
        must_succeed("if true { return 1 } else { return 2 }"),
        Value::Int(1),
    );
}

#[test]
fn if_two_armed_returns_else() {
    assert_eq!(
        must_succeed("if false { return 1 } else { return 2 }"),
        Value::Int(2),
    );
}

// ── if: elsif chains ────────────────────────────────────────────────────

#[test]
fn if_elsif_first_branch() {
    assert_eq!(
        must_succeed("if true { return a } elsif true { return b } else { return c }"),
        Value::String("a".into()),
    );
}

#[test]
fn if_elsif_second_branch() {
    assert_eq!(
        must_succeed("if false { return a } elsif true { return b } else { return c }"),
        Value::String("b".into()),
    );
}

#[test]
fn if_elsif_else_branch() {
    assert_eq!(
        must_succeed("if false { return a } elsif false { return b } else { return c }"),
        Value::String("c".into()),
    );
}

// ── if: U C generalisation ──────────────────────────────────────────────

#[test]
fn if_branches_returning_lambda() {
    // Branches are U(String → F String) — the result is a function.
    assert_eq!(
        must_succeed(
            "let f = if true { |x| return $x } else { |x| return nope }\nreturn !{f hello}"
        ),
        Value::String("hello".into()),
    );
}

#[test]
fn if_branches_returning_lambda_else() {
    assert_eq!(
        must_succeed(
            "let f = if false { |x| return nope } else { |x| return $x }\nreturn !{f world}"
        ),
        Value::String("world".into()),
    );
}

// ── if: expression conditions ───────────────────────────────────────────

#[test]
fn if_expr_condition() {
    assert_eq!(
        must_succeed("if $[1 + 1 == 2] { return yes } else { return no }"),
        Value::String("yes".into()),
    );
}

#[test]
fn if_command_condition() {
    // Condition is a command that returns Bool.
    assert_eq!(
        must_succeed("if !{equal hello hello} { return yes } else { return no }"),
        Value::String("yes".into()),
    );
}

#[test]
fn filter_predicate_must_return_bool() {
    // echo returns String, not Bool
    must_fail("filter { |x| echo $x } [a, b]");
}

// ── Variable scoping ─────────────────────────────────────────────────────

#[test]
fn undefined_variable_is_error() {
    must_fail("echo $nonexistent");
}

#[test]
fn shadowing_preserves_old_binding() {
    assert_eq!(
        must_succeed("let xv = 5\nlet f = { return $xv }\nlet xv = 10\n!$f"),
        Value::Int(5)
    );
}

#[test]
fn block_scoping() {
    // Variable defined inside a block is not visible outside
    must_fail("for [1] { |x| tmp = $x }\necho $tmp");
}

#[test]
fn assignment_returns_status_not_value() {
    // Assignment doesn't leak the value.
    // A block like { x = 42 } doesn't auto-execute 42.
    must_succeed("let xv = {echo hello}\necho 'after'");
}

#[test]
fn wildcard_assignment_discards_value() {
    assert_eq!(
        must_succeed("let _ = 42\nreturn ok"),
        Value::String("ok".into())
    );
}

#[test]
fn wildcard_destructure_discards_element() {
    assert_eq!(
        must_succeed("let [_, xv] = [1, 2]\nreturn $xv"),
        Value::Int(2)
    );
}

// ── Recursion ────────────────────────────────────────────────────────────

#[test]
fn self_recursion_works() {
    must_succeed("let f = { |n| if $[$n == 0] { echo done } else { f $[$n - 1] } }\nf 5");
}

#[test]
fn recursion_base_case() {
    assert_eq!(
        must_succeed(
            "let f = { |n| if $[$n == 0] { return 0 } else { let prev = f $[$n - 1]; return $[$n + $prev] } }\n!{f 3}"
        ),
        Value::Int(6) // 3 + 2 + 1 + 0
    );
}

// ── Mutual recursion ─────────────────────────────────────────────────────

#[test]
fn mutual_recursion_accumulator() {
    // even-sum passes n through odd-sum alternately, adding n only on even turns.
    // even-sum 0 0 → odd-sum 1 0 → even-sum 2 0 → odd-sum 3 2 → ... → 0+2+4+6+8+10 = 30.
    // Also exercises TCO: 12 tail calls for n=10.
    assert_eq!(
        must_succeed(concat!(
            "let even-sum = { |n acc| if $[$n > 10] { return $acc } else { odd-sum $[$n + 1] $[$acc + $n] } }\n",
            "let odd-sum  = { |n acc| if $[$n > 10] { return $acc } else { even-sum $[$n + 1] $acc } }\n",
            "!{even-sum 0 0}"
        )),
        Value::Int(30)
    );
}

#[test]
fn mutual_recursion_three_functions_compute_value() {
    // Three-way cycle: a adds 1, b adds 10, c adds 100 per step.
    // a 9 0 → b 8 1 → c 7 11 → a 6 111 → b 5 112 → c 4 122
    //       → a 3 222 → b 2 223 → c 1 233 → a 0 333 → 333.
    assert_eq!(
        must_succeed(concat!(
            "let a1 = { |n acc| if $[$n <= 0] { return $acc } else { b1 $[$n - 1] $[$acc +   1] } }\n",
            "let b1 = { |n acc| if $[$n <= 0] { return $acc } else { c1 $[$n - 1] $[$acc +  10] } }\n",
            "let c1 = { |n acc| if $[$n <= 0] { return $acc } else { a1 $[$n - 1] $[$acc + 100] } }\n",
            "!{a1 9 0}"
        )),
        Value::Int(333)
    );
}

#[test]
fn mutual_recursion_non_tail() {
    // Non-tail calls: f and g each add n to the result of calling the other.
    // f 4 = 4 + g 3 = 4 + (3 + f 2) = 4 + 3 + (2 + g 1) = 4 + 3 + 2 + (1 + f 0) = 11.
    assert_eq!(
        must_succeed(concat!(
            "let ff = { |n| if $[$n <= 0] { return 1 } else { let r = gg $[$n - 1]; return $[$n + $r] } }\n",
            "let gg = { |n| if $[$n <= 0] { return 1 } else { let r = ff $[$n - 1]; return $[$n + $r] } }\n",
            "!{ff 4}"
        )),
        Value::Int(11)
    );
}

// ── Type errors on wrong argument types ──────────────────────────────────

#[test]
fn index_string_not_indexable() {
    must_fail("let x = 'hello'\necho $x[0]");
}

#[test]
fn index_int_not_indexable() {
    must_fail("let x = 42\necho $x[0]");
}

#[test]
fn index_out_of_bounds() {
    must_fail("let items = [a, b]\necho $items[5]");
}

#[test]
fn index_missing_key() {
    must_fail("let m = [a: 1]\necho $m[b]");
}

#[test]
fn spread_non_list_in_list() {
    must_fail("[...'hello']");
}

#[test]
fn list_cons_spread() {
    // [x, ...xs] — cons.  Verifies surface semantics whether or not the
    // COW fast path fires.
    assert_eq!(
        must_succeed("return [1, ...[2, 3]]"),
        must_succeed("return [1, 2, 3]"),
    );
}

#[test]
fn list_snoc_spread() {
    // [...xs, x] — snoc.
    assert_eq!(
        must_succeed("return [...[1, 2], 3]"),
        must_succeed("return [1, 2, 3]"),
    );
}

#[test]
fn cons_spread_does_not_mutate_source() {
    // `[0, ...$xs]` constructs a new list; `$xs` itself is unchanged.
    // Holds unconditionally: the persistent representation path-copies on
    // mutation, so source aliasing is safe by construction.
    let v = must_succeed("let xs = [1, 2, 3]\nlet ys = [0, ...$xs]\nreturn $xs");
    assert_eq!(v, must_succeed("return [1, 2, 3]"));
}

#[test]
fn snoc_spread_does_not_mutate_source() {
    let v = must_succeed("let xs = [1, 2, 3]\nlet ys = [...$xs, 99]\nreturn $xs");
    assert_eq!(v, must_succeed("return [1, 2, 3]"));
}

#[test]
fn spread_non_map_in_map() {
    must_fail("[key: val, ...'hello']");
}

#[test]
fn map_spread_explicit_wins_when_spread_first() {
    // `[...$base, port: 9090]` — explicit field must override spread's port.
    let v = must_succeed(
        "let base = [port: 80, host: localhost]\nlet r = [...$base, port: 9090]\nreturn $r[port]",
    );
    assert_eq!(v, Value::Int(9090));
}

#[test]
fn map_spread_explicit_wins_when_spread_last() {
    // `[port: 9090, ...$base]` — original order; explicit must still win.
    let v = must_succeed(
        "let base = [port: 80, host: localhost]\nlet r = [port: 9090, ...$base]\nreturn $r[port]",
    );
    assert_eq!(v, Value::Int(9090));
}

#[test]
fn map_spread_non_overlapping_fields_accessible() {
    // Spread fields that don't conflict with explicit fields must be present.
    let v = must_succeed(
        "let base = [host: localhost, port: 80]\nlet r = [port: 9090, ...$base]\nreturn $r[host]",
    );
    assert_eq!(v, Value::String("localhost".into()));
}

#[test]
fn map_multiple_spreads_explicit_wins() {
    // With two spreads, the explicit field must still take priority.
    let v = must_succeed(
        "let a = [x: 1, z: 10]\nlet bb = [y: 2, z: 20]\nlet r = [...$a, ...$bb, z: 99]\nreturn $r[z]",
    );
    assert_eq!(v, Value::Int(99));
}

#[test]
fn destructure_list_from_non_list() {
    must_fail("let [a, b] = 'hello'");
}

#[test]
fn destructure_map_from_non_map() {
    must_fail("let [a: x] = [1, 2]");
}

#[test]
fn destructure_too_few_values() {
    must_fail("let [a, b, c] = [1, 2]");
}

#[test]
fn destructure_too_many_values() {
    must_fail("let [a, b] = [1, 2, 3]");
}

#[test]
fn not_callable_int() {
    must_fail("let f = 42\nf 1 2 3");
}

#[test]
fn not_callable_list() {
    must_fail("let f = [1, 2]\nf 1 2");
}

// ── is-empty type strictness ─────────────────────────────────────────────

#[test]
fn is_empty_on_empty_list() {
    assert_eq!(must_succeed("!{is-empty []}"), Value::Bool(true));
}

#[test]
fn is_empty_on_nonempty_list() {
    assert_eq!(must_succeed("!{is-empty [1, 2]}"), Value::Bool(false));
}

#[test]
fn is_empty_on_empty_map() {
    assert_eq!(must_succeed("!{is-empty [:]}"), Value::Bool(true));
}

#[test]
fn is_empty_on_string_checks_length() {
    assert_eq!(must_succeed("!{is-empty ''}"), Value::Bool(true));
    assert_eq!(must_succeed("!{is-empty 'hello'}"), Value::Bool(false));
}

#[test]
fn is_empty_on_int_is_type_error() {
    must_fail("is-empty 42");
}

// ── Filesystem predicates ────────────────────────────────────────────────

#[test]
fn exists_on_real_path() {
    let dir = std::env::temp_dir();
    let path = dir.to_string_lossy().replace('\\', "/");
    let script = format!("!{{exists '{}'}}", path);
    assert_eq!(must_succeed(&script), Value::Bool(true));
}

#[test]
fn exists_on_nonexistent() {
    assert_eq!(
        must_succeed("!{exists /nonexistent_path_xyz}"),
        Value::Bool(false)
    );
}

#[test]
fn is_dir_on_dir() {
    let dir = std::env::temp_dir();
    let path = dir.to_string_lossy().replace('\\', "/");
    let script = format!("!{{is-dir '{}'}}", path);
    assert_eq!(must_succeed(&script), Value::Bool(true));
}

#[test]
fn is_file_on_dir() {
    assert_eq!(must_succeed("!{is-file /tmp}"), Value::Bool(false));
}

// ── Error handling ───────────────────────────────────────────────────────

#[test]
fn try_error_map_has_status() {
    // try's handler receives a flat record: [cmd, status, message, line, col].
    assert_eq!(
        must_succeed("try { cat /nonexistent 2> /dev/null } { |err| return \"$err[status]\" }"),
        Value::String("1".into())
    );
}

#[test]
fn fail_propagates_without_try() {
    must_fail("fail [status: 1]");
}

// ── Functional builtins ──────────────────────────────────────────────────

#[test]
fn map_returns_list() {
    let result = must_succeed("!{map { |x| return $[$x * 2] } [1, 2, 3]}");
    assert_eq!(
        result,
        Value::list(vec![Value::Int(2), Value::Int(4), Value::Int(6),])
    );
}

#[test]
fn filter_returns_list() {
    let result = must_succeed("!{filter { |x| return $[$x > 2] } [1, 2, 3, 4]}");
    assert_eq!(result, Value::list(vec![Value::Int(3), Value::Int(4),]));
}

#[test]
fn fold_accumulates() {
    assert_eq!(
        must_succeed("!{fold { |acc x| return $[$acc + $x] } 0 [1, 2, 3]}"),
        Value::Int(6)
    );
}

#[test]
fn reduce_no_init() {
    assert_eq!(
        must_succeed("!{reduce { |a b| return $[$a + $b] } [1, 2, 3]}"),
        Value::Int(6)
    );
}

#[test]
fn reduce_empty_list_is_error() {
    must_fail("reduce { |a b| return $[$a + $b] } []");
}

#[test]
fn map_on_non_list_is_error() {
    must_fail("map { |x| return $x } 'hello'");
}

#[test]
fn for_iterates() {
    must_succeed("for [1, 2, 3] { |x| echo $x }");
}

// ── String builtins ──────────────────────────────────────────────────────

#[test]
fn len_string() {
    assert_eq!(must_succeed("!{length 'hello'}"), Value::Int(5));
}

#[test]
fn len_list() {
    assert_eq!(must_succeed("!{length [a, b, c]}"), Value::Int(3));
}

#[test]
fn len_on_int_is_error() {
    must_fail("length 42");
}

#[test]
fn upper_lower() {
    assert_eq!(
        must_succeed("!{upper 'hello'}"),
        Value::String("HELLO".into())
    );
    assert_eq!(
        must_succeed("!{lower 'HELLO'}"),
        Value::String("hello".into())
    );
}

#[cfg(feature = "grep")]
#[test]
fn replace_basic() {
    assert_eq!(
        must_succeed("!{re-replace 'world' 'al' 'hello world'}"),
        Value::String("hello al".into())
    );
}

#[test]
fn string_replace_passes_regex_metacharacters_through() {
    // `{` would be a regex error under re-replace, but string-replace
    // takes it as a literal — this is the bug file-replace-string ran into.
    assert_eq!(
        must_succeed("!{string-replace '{x}' '<x>' 'a {x} b'}"),
        Value::String("a <x> b".into())
    );
}

#[test]
fn string_replace_errors_when_absent() {
    must_fail("!{string-replace 'zzz' 'qqq' 'hello world'}");
}

#[cfg(feature = "grep")]
#[test]
fn split_and_join() {
    must_succeed("let parts = re-split '/' '/usr/local/bin'\necho !{intercalate '-' $parts}");
}

#[test]
fn has_on_map() {
    assert_eq!(must_succeed("!{has [a: 1, b: 2] a}"), Value::Bool(true));
    assert_eq!(must_succeed("!{has [a: 1, b: 2] c}"), Value::Bool(false));
}

// ── Scoped effects ───────────────────────────────────────────────────────

#[test]
fn with_overrides_command() {
    // within handlers replace commands at head-dispatch (SPEC §4.1)
    must_succeed("within [handlers: [cat: { |args| echo mocked }]] { cat /nonexistent }");
}

#[test]
fn with_does_not_leak() {
    // After within block, the handler is gone
    must_succeed("within [handlers: [mytest: { |args| echo mock }]] { mytest }\necho 'after with'");
}

#[cfg(unix)]
#[test]
fn grant_exec_subcommand_allows_listed_subcommand() {
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([(
                "/bin/sh".into(),
                ExecPolicy::Subcommands(BTreeSet::from(["-c".into()])),
            )]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    let args = vec!["-c".into(), "exit 0".into()];
    shell
        .with_capabilities(grant, |shell| {
            shell.check_exec_args("/bin/sh", &["/bin/sh"], &args)
        })
        .expect("-c should be allowed");
}

#[cfg(unix)]
#[test]
fn grant_exec_subcommand_denies_unlisted_subcommand() {
    must_fail("grant [exec: ['/bin/sh': [-c]]] { /bin/sh -s }");
}

#[test]
fn grant_exec_thunk_form_errors_with_clear_message() {
    // Regression guard: the removed thunk form should produce a clear error.
    must_fail("grant [exec: [cmd: { return ok }]] { cmd }");
}

#[cfg(unix)]
#[test]
fn within_handler_applies_inside_pipeline() {
    assert_eq!(
        must_succeed(
            "within [handlers: [cat: { |args| echo mocked }]] { let n = !{cat /nonexistent | from-string | length}; return $n }"
        ),
        Value::Int(7)
    );
}

#[test]
fn within_dir_scoped() {
    let tmp = std::env::temp_dir();
    must_succeed(&format!(
        "within [dir: '{}'] {{ echo 'in tmp' }}",
        tmp.display()
    ));
}

#[test]
fn within_dir_resolves_relative_builtin_paths() {
    let dir = std::env::temp_dir().join(format!("ral-within-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let script = format!(
        "within [dir: '{}'] {{ \
             to-string hello > 'note.txt'; \
             let wd = cwd; \
             let txt = !{{from-string < 'note.txt'}}; \
             let matches = glob '*.txt'; \
             return [cwd: $wd, txt: $txt, exists: !{{exists 'note.txt'}}, count: !{{length $matches}}] \
         }}",
        dir.display()
    );
    let result = must_succeed(&script);
    assert_eq!(
        map_field(&result, "cwd"),
        Value::String(dir.display().to_string())
    );
    assert_eq!(map_field(&result, "txt"), Value::String("hello".into()));
    assert_eq!(map_field(&result, "exists"), Value::Bool(true));
    assert_eq!(map_field(&result, "count"), Value::Int(1));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn within_dir_nonexistent_is_error() {
    must_fail("within [dir: '/nonexistent_dir_xyz'] { echo bad }");
}

/// `PWD` / `OLDPWD` are derived from the shell's working directory and
/// live on `context.cwd`, not in `env_overrides`.  Setting them through
/// `within [env: …]` is rejected in every build profile, and the message
/// points the user at `cd`.  Running under `--release` pins that the
/// rejection is a real returned error.
#[test]
fn within_env_rejects_pwd() {
    for key in ["PWD", "OLDPWD"] {
        let src = format!("within [env: [{key}: '/tmp']] {{ echo bad }}");
        match eval(&src) {
            Err(Break::Error(err)) => {
                assert!(
                    err.message.contains(key) && err.message.contains("cd"),
                    "expected message naming {key} and `cd`, got: {}",
                    err.message
                );
            }
            other => panic!("within env: [{key}: …] should error, got {other:?}"),
        }
    }
}

/// External commands spawned inside `within [dir: X]` must run with `X`
/// as their cwd.  Captured via `!{pwd}`, which routes through the
/// bundled `pwd` (and, since `dynamic.ambient.cwd` is set, through the pipeline
/// helper subprocess that `apply_env` configures with `current_dir`).
/// On systems with firmlinks (macOS `/var` ↔ `/private/var`) we accept
/// either the raw or canonicalised form.
#[test]
fn within_dir_carries_to_external_command() {
    let dir = std::env::temp_dir().join(format!("ral-within-external-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());

    let script = format!(
        "within [dir: '{}'] {{ let out = !{{pwd}}; return $out }}",
        dir.display()
    );
    let result = must_succeed(&script);
    let _ = std::fs::remove_dir_all(&dir);

    let out = match result {
        Value::String(s) => s,
        other => panic!("expected pwd to return a String, got {other:?}"),
    };
    let trimmed = out.trim();
    let raw = dir.display().to_string();
    let canon = canonical.display().to_string();
    assert!(
        trimmed == raw || trimmed == canon,
        "pwd inside within: expected {raw:?} or {canon:?}, got {trimmed:?}",
    );
}

#[test]
fn grant_fs_read_denies_builtin_read() {
    let dir = std::env::temp_dir().join(format!("ral-grant-read-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let allowed = dir.join("allowed");
    let denied = dir.join("denied");
    std::fs::write(&denied, "secret").unwrap();
    // Read goes through the redirect path (`from-string < $path`); the
    // capability layer is consulted in `open_file` regardless of the
    // builtin doing the actual read.
    let script = format!(
        "grant [fs: [read: ['{}']]] {{ from-string < '{}' }}",
        allowed.display(),
        denied.display()
    );
    must_fail(&script);
    let _ = std::fs::remove_file(&denied);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `deny` is symmetric: a region named in the deny list blocks
/// reads as well as writes.  This test puts a file inside an
/// otherwise readable region and a deny entry on the file; the
/// read must fail.  Earlier shapes only consulted deny_paths on
/// writes — this regression-locks the symmetric semantics.
#[cfg(unix)]
#[test]
fn grant_fs_deny_blocks_reads() {
    let dir = std::env::temp_dir().join(format!("ral-deny-read-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let target = dir.join("secret.txt");
    std::fs::write(&target, "shh").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}'], deny: ['{}']]] {{ from-string < '{}' }}",
        dir.display(),
        target.display(),
        target.display(),
    );
    must_fail(&script);
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `deny` matches by containment, not equality.  A deny entry
/// for a directory blocks every path beneath it — the rule the
/// SPEC describes.  This test admits the parent of two files
/// for read, denies the directory itself, and asserts that a
/// read of one of the files fails.
#[cfg(unix)]
#[test]
fn grant_fs_deny_covers_subpaths_of_a_directory() {
    let outer = std::env::temp_dir().join(format!("ral-deny-dir-{}", std::process::id()));
    let inner = outer.join("forbidden");
    let _ = std::fs::create_dir_all(&inner);
    let leaf = inner.join("leaf.txt");
    std::fs::write(&leaf, "no").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}'], deny: ['{}']]] {{ from-string < '{}' }}",
        outer.display(),
        inner.display(),
        leaf.display(),
    );
    must_fail(&script);
    let _ = std::fs::remove_dir_all(&outer);
}

/// `glob` gates every match, not just the pattern: a deny on a
/// subpath drops the matching hits while readable siblings survive.
/// `checked_read_path` admits the pattern's parent, so without the
/// per-match gate the denied file would be enumerated and returned —
/// an unchecked in-process read of a denied path.
#[cfg(unix)]
#[test]
fn grant_fs_deny_omits_glob_matches() {
    let dir = std::env::temp_dir().join(format!("ral-deny-glob-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let visible = dir.join("visible.txt");
    let secret = dir.join("secret.txt");
    std::fs::write(&visible, "ok").unwrap();
    std::fs::write(&secret, "shh").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}'], deny: ['{}']]] {{ glob '{}/*.txt' }}",
        dir.display(),
        secret.display(),
        dir.display(),
    );
    let out = format!("{:?}", must_succeed(&script));
    assert!(out.contains("visible.txt"), "readable match must survive");
    assert!(!out.contains("secret.txt"), "denied match must be omitted");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `list-dir` gates every entry, not just the directory: a deny on a
/// subpath drops that entry (and its size/mtime metadata) while
/// readable siblings survive.  Without the per-entry gate the denied
/// file would be stat'd and returned.
#[cfg(unix)]
#[test]
fn grant_fs_deny_omits_list_dir_entries() {
    let dir = std::env::temp_dir().join(format!("ral-deny-list-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let visible = dir.join("visible.txt");
    let secret = dir.join("secret.txt");
    std::fs::write(&visible, "ok").unwrap();
    std::fs::write(&secret, "shh").unwrap();
    let script = format!(
        "grant [fs: [read: ['{}'], deny: ['{}']]] {{ list-dir '{}' }}",
        dir.display(),
        secret.display(),
        dir.display(),
    );
    let out = format!("{:?}", must_succeed(&script));
    assert!(out.contains("visible.txt"), "readable entry must survive");
    assert!(!out.contains("secret.txt"), "denied entry must be omitted");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cwd-relative patterns return cwd-relative matches, so
/// `glob '*.rs' | each { |f| ... $f ... }` composes.
#[cfg(unix)]
#[test]
fn glob_relative_pattern_returns_relative_paths() {
    let dir = std::env::temp_dir().join(format!("ral-glob-rel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("b.txt"), "").unwrap();

    let script = format!("within [dir: '{}'] {{ glob '*.txt' }}", dir.display());
    let items = match must_succeed(&script) {
        Value::List(xs) => xs,
        other => panic!("glob list: unexpected {other:?}"),
    };
    let names: Vec<String> = items.iter().map(std::string::ToString::to_string).collect();
    assert_eq!(names, vec!["a.txt".to_string(), "b.txt".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Dotfiles are excluded from wildcard matches — `*` does not
/// consume a leading dot.  A fully-literal name still finds the
/// file.  Wildcard dotfile matches (`.h*`, `.*.txt`) are not
/// supported by the underlying crate; callers wanting those should
/// use `list-dir | filter`.
#[cfg(unix)]
#[test]
fn glob_excludes_dotfiles_from_wildcard_matches() {
    let dir = std::env::temp_dir().join(format!("ral-glob-dot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join(".hidden.txt"), "").unwrap();

    let script_star = format!("within [dir: '{}'] {{ glob '*.txt' }}", dir.display());
    let star = match must_succeed(&script_star) {
        Value::List(xs) => xs,
        other => panic!("glob list: unexpected {other:?}"),
    };
    let names: Vec<String> = star.iter().map(std::string::ToString::to_string).collect();
    assert_eq!(names, vec!["a.txt".to_string()]);

    let script_literal = format!("within [dir: '{}'] {{ glob '.hidden.txt' }}", dir.display());
    let lit = match must_succeed(&script_literal) {
        Value::List(xs) => xs,
        other => panic!("glob list: unexpected {other:?}"),
    };
    let names: Vec<String> = lit.iter().map(std::string::ToString::to_string).collect();
    assert_eq!(names, vec![".hidden.txt".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Absolute input → absolute output; shape preservation is symmetric.
#[cfg(unix)]
#[test]
fn glob_absolute_pattern_returns_absolute_paths() {
    let dir = std::env::temp_dir().join(format!("ral-glob-abs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "").unwrap();

    let script = format!("glob '{}/*.txt'", dir.display());
    let items = match must_succeed(&script) {
        Value::List(xs) => xs,
        other => panic!("glob list: unexpected {other:?}"),
    };
    let expected = dir.join("a.txt").display().to_string();
    let names: Vec<String> = items.iter().map(std::string::ToString::to_string).collect();
    assert_eq!(names, vec![expected]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `glob` runs through the same `Resolver::lex` pipeline that
/// every other path-taking builtin uses, so `~` and `xdg:` at
/// the head of a pattern expand before the glob crate sees it.
/// Regression: a previous shape would have fed the literal
/// `~/...` to glob and quietly matched nothing.
#[cfg(unix)]
#[test]
fn glob_expands_tilde_in_pattern() {
    let dir = std::env::temp_dir().join(format!("ral-glob-tilde-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("b.txt"), "").unwrap();

    // Point HOME at the tempdir through a `within [env: …]` override so
    // `~/*.txt` resolves into it.  Scoping HOME dynamically keeps the test
    // hermetic — no process-global env mutation to race the other tests
    // under RUST_TEST_THREADS.
    let src = format!(
        "within [env: [HOME: '{}']] {{ let xs = glob \"~/*.txt\"; return $xs }}",
        dir.display()
    );
    let result = eval(&src);

    let _ = std::fs::remove_dir_all(&dir);
    let items = match result {
        Ok(Value::List(xs)) => xs,
        other => panic!("glob with ~ pattern: unexpected {other:?}"),
    };
    let names: Vec<String> = items.iter().map(std::string::ToString::to_string).collect();
    assert!(
        names.iter().any(|n| n.ends_with("/a.txt")),
        "expected /a.txt in {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("/b.txt")),
        "expected /b.txt in {names:?}"
    );
}

#[cfg(unix)]
#[test]
fn grant_fs_write_denies_external_redirect() {
    let dir = std::env::temp_dir().join(format!("ral-grant-write-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let allowed = dir.join("allowed");
    let denied = dir.join("denied.txt");
    let script = format!(
        "grant [fs: [write: ['{}']]] {{ /bin/echo hi > '{}' }}",
        allowed.display(),
        denied.display()
    );
    must_fail(&script);
    let _ = std::fs::remove_file(&denied);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── within handler nesting ───────────────────────────────────────────────

#[test]
fn within_handler_inner_shadows_outer_same_name() {
    // Innermost per-name handler wins when both frames name the same command.
    assert_eq!(
        must_succeed(
            "within [handlers: [cmd: { |args| echo outer; return 'outer' }]] { within [handlers: [cmd: { |args| echo inner; return 'inner' }]] { cmd } }"
        ),
        Value::String("inner".into())
    );
}

#[test]
fn within_handler_outer_fires_when_inner_does_not_match() {
    // Inner frame has no entry for cmd2; outer frame fires.
    assert_eq!(
        must_succeed(
            "within [handlers: [cmd2: { |args| echo outer; return 'outer' }]] { within [handlers: [cmd1: { |args| echo inner; return 'inner' }]] { cmd2 } }"
        ),
        Value::String("outer".into())
    );
}

#[test]
fn outer_per_name_beats_inner_catch_all() {
    // Per-name match beats catch-all across the whole stack — outer per-name
    // `other` is preferred over an inner frame's catch-all.
    assert_eq!(
        must_succeed(
            "within [handlers: [other: { |args| echo outer; return 'outer' }]] { within [handler: { |n _a| return 'catch' }] { other } }"
        ),
        Value::String("outer".into())
    );
}

#[test]
fn within_catch_all_skips_builtins() {
    // Catch-all handler must NOT intercept builtins — they are language-internal.
    assert_eq!(
        must_succeed("within [handler: { |n _a| return 'caught' }] { length [1, 2, 3] }"),
        Value::Int(3)
    );
}

#[test]
fn alias_inside_within_shadows_within_per_name() {
    // Last-pushed wins now: an alias installed inside a within body sits
    // above the within frame and shadows its per-name entry.  Before the
    // FrameOrigin removal, the [Layer*, Alias*, Within*] invariant put
    // the alias under the within and the within's entry won.
    assert_eq!(
        must_succeed(
            "within [handlers: [foo: { |args| echo A; return 'A' }]] { alias foo { |args| echo B; return 'B' }; foo }"
        ),
        Value::String("B".into())
    );
}

// ── handler / alias mode preservation at install ─────────────────────────────
//
// A computed `within [handlers: $h]` opts map and a runtime-installed alias
// are invisible to the static check, so their arms' `PipeSpec`s are pinned to
// the head's spec at install instead.  An unknown head's spec is fully fresh,
// so the arm defines its modes and a value-output arm installs and runs; a
// known head with incompatible modes is the clash rejected there.

/// A computed (non-literal) `within` opts map carrying a value-output arm
/// defines the unknown head's modes, so it installs and runs, yielding the
/// arm's value.
#[test]
fn computed_within_value_output_arm_runs() {
    assert_eq!(
        must_succeed("let h = [foo: { |args| return 3 }]; within [handlers: $h] { foo }"),
        Value::Int(3)
    );
}

/// A computed `within` opts map carrying a byte-output arm defines the unknown
/// head as byte-output and runs.
#[test]
fn computed_within_byte_output_arm_runs() {
    // `foo` emits bytes; the block captures nothing and yields unit.
    assert_eq!(
        must_succeed("let h = [foo: { |args| echo hi }]; within [handlers: $h] { foo }"),
        Value::Unit
    );
}

/// A runtime-installed value-output alias defines the unknown head's modes, so
/// the install guard in `Shell::install_alias` accepts it.
#[test]
fn value_output_alias_installs() {
    assert_eq!(must_succeed("alias foo { |args| return 3 }"), Value::Unit);
}

#[cfg(unix)]
#[test]
fn grant_exec_attenuation_subcommand_intersection_permits_common() {
    // Intersection of [-c, -s] and [-c] permits -c.
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    let outer = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([(
                "/bin/sh".into(),
                ExecPolicy::Subcommands(BTreeSet::from(["-c".into(), "-s".into()])),
            )]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    let inner = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([(
                "/bin/sh".into(),
                ExecPolicy::Subcommands(BTreeSet::from(["-c".into()])),
            )]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    let args = vec!["-c".into(), "exit 0".into()];
    shell
        .with_capabilities(outer, |shell| {
            shell.with_capabilities(inner, |shell| {
                shell.check_exec_args("/bin/sh", &["/bin/sh"], &args)
            })
        })
        .expect("-c should remain allowed by the intersection");
}

#[cfg(unix)]
#[test]
fn grant_exec_attenuation_subcommand_intersection_denies_outer_only() {
    // -s is in the outer list but not the inner; intersection denies it.
    must_fail(
        "grant [exec: ['/bin/sh': [-c, -s]]] { grant [exec: ['/bin/sh': [-c]]] { /bin/sh -s } }",
    );
}

// ── Edge cases that must not panic ───────────────────────────────────────

#[test]
fn empty_block() {
    must_not_panic("{}");
}

#[test]
fn empty_lambda_call() {
    must_not_panic("let f = { |_| }\nf 0");
}

#[test]
fn assign_block_does_not_execute() {
    // Assigning a block should NOT execute it
    must_succeed("let xv = { fail [status: 1] }\necho 'survived'");
}

#[test]
fn deeply_nested_calls() {
    // Tree-walker uses Rust's call stack — deep recursion is limited.
    // Each level uses eval_stmts → eval_command → apply_lambda_frame → Shell::with_thunk_body → eval_stmts.
    must_succeed(
        "let f = { |n| if $[$n == 0] { return 0 } else { let prev = f $[$n - 1]; return $prev } }\nf 10",
    );
}

#[test]
fn script_args_are_not_polluted_by_runner_argv() {
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    shell.set_args(vec!["alpha".into(), "beta".into()]);
    builtins::register(&mut shell, common::prelude_comp());
    let result = evaluate(
        &std::sync::Arc::new(elaborate(
            &parse("return $args").unwrap(),
            std::collections::HashSet::default(),
        )),
        &mut shell,
    )
    .expect("evaluate $args");
    assert_eq!(
        result,
        Value::list(vec![
            Value::String("alpha".into()),
            Value::String("beta".into())
        ])
    );
}

#[test]
fn env_overrides_shadow_process_env_in_dollar_env() {
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    shell.set_env_var("RAL_TEST_ENV", "override");
    builtins::register(&mut shell, common::prelude_comp());
    let result = evaluate(
        &std::sync::Arc::new(elaborate(
            &parse("return $env[RAL_TEST_ENV]").unwrap(),
            std::collections::HashSet::default(),
        )),
        &mut shell,
    )
    .expect("evaluate $env");
    assert_eq!(result, Value::String("override".into()));
}

#[test]
fn many_variables() {
    let mut script = String::new();
    for i in 0..100 {
        let _ = writeln!(script, "let x{i} = {i}");
    }
    script.push_str("echo $x99\n");
    must_succeed(&script);
}

#[test]
fn list_of_lambdas() {
    must_succeed(
        "let fns = [{ |x| return $[$x + 1] }, { |x| return $[$x * 2] }]\necho !{$fns[0] 5}",
    );
}

#[test]
fn map_of_lambdas() {
    must_succeed(
        "let ops = [inc: { |x| return $[$x + 1] }, dbl: { |x| return $[$x * 2] }]\necho !{$ops[inc] 5}",
    );
}

#[test]
fn interpolation_with_all_forms() {
    must_succeed("let xv = 5\necho \"val=$xv arith=$[$xv + 1] sub=!{echo hi}\"");
}

#[test]
fn on_exit_runs() {
    // exit always returns Err(Break::Escape(Escape::Exit(_))) — callers decide whether to treat it as clean.
    let result = eval("exit 0");
    match result {
        Err(Break::Escape(ral_core::types::Escape::Exit(0))) => {}
        other => panic!("expected exit 0, got: {other:?}"),
    }
}

#[test]
fn exit_rejects_non_integer_status() {
    must_fail("exit nope");
}

#[test]
fn retry_exhaustion() {
    // false is a value, not a failure. Use fail for actual failure.
    must_fail("retry 3 { fail [status: 1] }");
}

// Variant-dispatch `case` coverage lives in core/tests/typecheck.rs and
// ral/tests/variants.rs.

#[test]
fn spread_in_command() {
    must_succeed("let args = [hello, world]\necho ...$args");
}

// ── New prelude functions ───────────────────────────────────────────────

#[cfg(feature = "grep")]
#[test]
fn words_splits_on_space() {
    assert_eq!(
        must_succeed("!{words 'hello world foo'}"),
        Value::list(vec![
            Value::String("hello".into()),
            Value::String("world".into()),
            Value::String("foo".into()),
        ])
    );
}

// ── TCO (tail-call optimization) ────────────────────────────────────────

#[test]
fn tco_deep_recursion() {
    // 10000 recursive calls would overflow without TCO (default 8MB stack ≈ 4000 frames).
    must_succeed(
        "let countdown = { |n| if $[$n <= 0] { return done } else { countdown $[$n - 1] } }\necho !{countdown 10000}",
    );
}

#[test]
fn tco_within_handler_non_tail() {
    // handler called NOT in tail position must execute, not escape as TailCall.
    assert_eq!(
        must_succeed(
            "within [handlers: [cmd: { |args| echo 6; return 6 }]] { let y = cmd; return $y }"
        ),
        Value::Int(6)
    );
}

#[test]
fn tco_if_condition_not_tail() {
    // The condition of `if` must NOT be in tail position.
    // f returns Bool; if `if` treated it as tail, TailCall would escape.
    assert_eq!(
        must_succeed(
            "let check = { |x| return $[$x > 0] }\nif !{check 5} { return yes } else { return no }"
        ),
        Value::String("yes".into())
    );
}

// ── Arithmetic indexing ─────────────────────────────────────────────────

#[test]
fn arith_index_in_comparison() {
    assert_eq!(
        must_succeed("let m = [status: 0]\nreturn $[$m[status] == 0]"),
        Value::Bool(true)
    );
}

// ── $nproc ──────────────────────────────────────────────────────────────

#[test]
fn nproc_is_positive_int() {
    let val = must_succeed("return $nproc");
    match val {
        Value::Int(n) => assert!(n > 0, "nproc should be positive"),
        _ => panic!("nproc should be Int, got {val:?}"),
    }
}

// ── echo returns its string ─────────────────────────────────────────────

#[test]
fn echo_returns_unit() {
    assert_eq!(must_succeed("!{echo hello}"), Value::Unit);
}

#[test]
fn echo_side_effect_only() {
    // echo prints to stdout, returns Unit. The value is the side effect.
    assert_eq!(must_succeed("echo hello world"), Value::Unit);
}

// ── assert_eq (user-defined, not in prelude) ────────────────────────────

const ASSERT_EQ_DEF: &str = "
let assert_eq = { |name expected actual|
    if !{equal $expected $actual} {} else {
        echo 'assert_eq mismatch' 1>&2
        fail [status: 1]
    }
}";

#[test]
fn assert_eq_passes() {
    must_succeed(&format!("{}\nassert_eq 'test' 42 42", ASSERT_EQ_DEF));
}

#[test]
fn assert_eq_fails() {
    must_fail(&format!("{}\nassert_eq 'test' 42 99", ASSERT_EQ_DEF));
}

// ── failure propagation ─────────────────────────────────────────────────

#[test]
fn failure_propagation_stops_sequence() {
    // cat /nonexistent fails → second echo should NOT run
    must_fail("cat /nonexistent 2> /dev/null; echo 'should not reach'");
}

#[test]
fn try_suppresses_failure_propagation() {
    must_succeed("try { cat /nonexistent 2> /dev/null } { |_| echo caught }");
}

#[cfg(feature = "coreutils")]
#[test]
fn bundled_uutils_capture_honours_scoped_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let script = format!(
        "within [dir: '{}'] {{ let out = pwd\nreturn $out }}",
        dir.path().display()
    );
    let Value::String(output) = must_succeed(&script) else {
        panic!("expected pwd output string");
    };
    assert_eq!(
        std::path::PathBuf::from(output).canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[cfg(feature = "coreutils")]
#[test]
fn bundled_uutils_capture_completes_with_buffer_sink() {
    let out = must_succeed("let out = ls -lah\nreturn $out");
    match out {
        Value::String(s) => assert!(!s.is_empty()),
        other => panic!("expected captured ls output, got {other:?}"),
    }
}

// ── §4.4 empty block returns Unit ───────────────────────────────────────

#[test]
fn empty_block_returns_unit() {
    // !{} forces an empty block — in CBPV, an empty thunk evaluates to Unit.
    // But !{{}} is force(thunk(return(thunk(empty)))) — returns a Block, not Unit.
    // The test should use !{} not !{{}}.
    let val = must_succeed("!{}");
    assert_eq!(val, Value::Unit);
}

// ── §4.6 reject complex types as external command args ──────────────────

#[test]
fn list_to_external_is_error() {
    must_fail("cat [1, 2, 3]");
}

#[test]
fn map_to_external_is_error() {
    must_fail("cat [a: 1]");
}

#[test]
fn lambda_to_external_is_error() {
    must_fail("let f = { |x| return $x }\ncat $f");
}

// ── §10.1 try cmd for runtime errors ────────────────────────────────────

#[test]
fn try_runtime_error_has_cmd_runtime() {
    // A pure-evaluator failure (divide by zero) leaves no failing
    // command in the audit subtree, so try's err.cmd reads the
    // placeholder "<runtime>".
    let result = must_succeed(
        "let r = try { let _ = $[1 / 0]\n return 'unreached' } { |e| return $e[cmd] }\n\
         return $r",
    );
    assert_eq!(result, Value::String("<runtime>".into()));
}

// ── §4 rule 2: block with trailing args ─────────────────────────────────

#[test]
fn block_with_trailing_args_is_error() {
    must_fail("let b = { echo hi }\n$b extra");
}

// ── §4.6 Currying / partial application ─────────────────────────────────

#[test]
fn curry_under_application() {
    // { |x y| ... } applied with 1 arg returns a lambda
    assert_eq!(
        must_succeed("let add = { |x y| return $[$x + $y] }\nlet add5 = add 5\n!{add5 3}"),
        Value::Int(8)
    );
}

#[test]
fn curry_exact_application() {
    assert_eq!(
        must_succeed("let add = { |x y| return $[$x + $y] }\n!{add 5 3}"),
        Value::Int(8)
    );
}

#[test]
fn curry_map_partial() {
    // Passing a function as data is explicit: map $upper $list
    assert_eq!(
        must_succeed("!{map $upper [hello, world]}"),
        Value::list(vec![
            Value::String("HELLO".into()),
            Value::String("WORLD".into())
        ])
    );
}

#[test]
fn bare_function_name_in_argument_position_is_literal() {
    must_fail("!{map upper [hello, world]}");
}

#[test]
fn return_bare_name_is_literal_even_when_prelude_binds_it() {
    assert_eq!(must_succeed("return upper"), Value::String("upper".into()));
}

#[test]
fn return_deref_name_is_bound_value() {
    let v = must_succeed("return $upper");
    assert!(
        matches!(v, Value::Lambda { .. } | Value::Block { .. }),
        "expected thunk, got {v:?}"
    );
}

#[test]
fn return_force_expression() {
    // return !{...} forces the block and returns its value; it is not
    // special-cased away from the general return-a-value path.
    assert_eq!(
        must_succeed("return !{upper hello}"),
        Value::String("HELLO".into())
    );
}

#[test]
fn head_position_still_calls_prelude_function() {
    assert_eq!(
        must_succeed("!{upper hello}"),
        Value::String("HELLO".into())
    );
}

#[test]
fn lexical_head_position_still_calls_bound_function() {
    assert_eq!(
        must_succeed("let f = { |x| return $[$x + 1] }\n!{f 4}"),
        Value::Int(5)
    );
}

#[test]
fn lexical_non_head_name_is_literal_without_deref() {
    assert_eq!(
        must_succeed("let f = { |x| return $x }\nreturn f"),
        Value::String("f".into())
    );
}

#[test]
fn lexical_non_head_name_uses_deref_to_get_value() {
    let v = must_succeed("let f = { |x| return $x }\nreturn $f");
    assert!(
        matches!(v, Value::Lambda { .. } | Value::Block { .. }),
        "expected thunk, got {v:?}"
    );
}

#[test]
fn binding_position_bare_name_dispatches_in_let_rhs() {
    assert_eq!(
        must_succeed("let upper = { |x| return $x }\nlet xv = upper hello\nreturn $xv"),
        Value::String("hello".into())
    );
}

#[test]
fn list_position_bare_name_stays_literal() {
    // In list position, bare `upper` is a string literal, not a variable lookup.
    assert_eq!(
        must_succeed("let upper = { |x| return $x }\nreturn [upper, hello]"),
        Value::list(vec![
            Value::String("upper".into()),
            Value::String("hello".into())
        ]),
    );
}

#[test]
fn list_position_deref_gives_thunk() {
    // $upper in list position is a variable lookup.
    let v = must_succeed("let upper = { |x| return $x }\nreturn [$upper]");
    match v {
        Value::List(items) => {
            assert!(
                matches!(items[0], Value::Lambda { .. } | Value::Block { .. }),
                "expected thunk, got {:?}",
                items[0]
            );
        }
        other => panic!("expected list, got {other:?}"),
    }
}

#[test]
fn map_position_bare_name_stays_literal() {
    let v = must_succeed("let upper = { |x| return $x }\nreturn [label: upper, fn: $upper]");
    match v {
        Value::Map(m) => {
            assert_eq!(m.get("label"), Some(&Value::String("upper".into())));
            assert!(
                matches!(
                    m.get("fn"),
                    Some(Value::Lambda { .. } | Value::Block { .. })
                ),
                "expected thunk under 'fn', got {:?}",
                m.get("fn")
            );
        }
        other => panic!("expected map, got {other:?}"),
    }
}

#[test]
fn filter_named_function_requires_deref() {
    assert_eq!(
        must_succeed("let pred = { |x| return $[$x > 1] }\n!{filter $pred [1, 2, 3]}"),
        Value::list(vec![Value::Int(2), Value::Int(3)])
    );
}

#[test]
fn filter_bare_named_function_argument_is_literal() {
    must_fail("let pred = { |x| return $[$x > 1] }\n!{filter pred [1, 2, 3]}");
}

#[test]
fn fold_named_function_requires_deref() {
    assert_eq!(
        must_succeed("let add = { |acc x| return $[$acc + $x] }\n!{fold $add 0 [1, 2, 3]}"),
        Value::Int(6)
    );
}

#[test]
fn fold_bare_named_function_argument_is_literal() {
    must_fail("let add = { |acc x| return $[$acc + $x] }\n!{fold add 0 [1, 2, 3]}");
}

#[test]
fn curry_three_params() {
    assert_eq!(
        must_succeed("let f = { |x y z| return $[$x + $y + $z] }\n!{f 1 2 3}"),
        Value::Int(6)
    );
}

#[test]
fn curry_three_partial() {
    assert_eq!(
        must_succeed("let f = { |x y z| return $[$x + $y + $z] }\nlet gg = f 1 2\n!{gg 3}"),
        Value::Int(6)
    );
}

// ── §4.4: empty block returns Unit ──────────────────────────────────────
// (already tested above as empty_block_returns_unit)

// ── §13 Concurrency: spawn returns structured values ────────────────────

#[test]
fn spawn_returns_list() {
    assert_eq!(
        must_succeed("let h = !{spawn { return [1, 2, 3] }}\nlet r = await $h\nreturn $r[value]"),
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
}

#[test]
fn spawn_returns_map() {
    let result = must_succeed(
        "let h = !{spawn { return [name: alice] }}\nlet r = await $h\nreturn $r[value][name]",
    );
    assert_eq!(result, Value::String("alice".into()));
}

#[test]
fn spawn_returns_lambda() {
    assert_eq!(
        must_succeed(
            "let h = !{spawn { return { |x| return $[$x * 2] } }}\nlet r = await $h\nlet dbl = $r[value]\n!{dbl 21}"
        ),
        Value::Int(42)
    );
}

#[test]
fn spawn_returns_int() {
    assert_eq!(
        must_succeed("let h = !{spawn { return 42 }}\nlet r = await $h\nreturn $r[value]"),
        Value::Int(42)
    );
}

#[test]
fn await_cached() {
    // Second await returns the same record from cache.
    assert_eq!(
        must_succeed(
            "let h = !{spawn { return 99 }}\nlet a = await $h\nlet bb = await $h\nreturn $[$a[value] + $bb[value]]"
        ),
        Value::Int(198)
    );
}

#[test]
fn background_let_binds_handle() {
    // `cmd &` suspends the pipeline as a thunk and binds the spawn handle;
    // `await` recovers the pipeline's return value through the record's
    // `value` field.
    assert_eq!(
        must_succeed("let h = echo hi | from-line &\nlet r = await $h\nreturn $r[value]"),
        Value::String("hi".into())
    );
}

#[test]
fn background_statement_runs_detached() {
    // A bare `cmd &` statement spawns the thunk and the sequence continues
    // without awaiting it.
    assert_eq!(
        must_succeed("echo detached &\nreturn done"),
        Value::String("done".into())
    );
}

#[test]
fn background_value_pipeline() {
    // The suspended body is a value pipeline, not an external command: the
    // handle's payload is the computed value.
    assert_eq!(
        must_succeed("let h = return 42 &\nlet r = await $h\nreturn $r[value]"),
        Value::Int(42)
    );
}

#[test]
fn par_returns_structured() {
    assert_eq!(
        must_succeed("!{par { |x| return $[$x * $x] } [1, 2, 3, 4] 2}"),
        Value::list(vec![
            Value::Int(1),
            Value::Int(4),
            Value::Int(9),
            Value::Int(16)
        ])
    );
}

// ── Concurrency stress tests ────────────────────────────────────────────

#[test]
fn spawn_closure_captures_survive() {
    // A lambda that closes over a value defined before spawn.
    // The closure must capture the binding and return a working lambda.
    assert_eq!(
        must_succeed(
            "let secret = 42\nlet h = !{spawn { return { |x| return $[$x + $secret] } }}\nlet r = await $h\nlet f = $r[value]\n!{f 8}"
        ),
        Value::Int(50)
    );
}

#[test]
fn spawn_nested_structured() {
    // Spawn returns a map containing a list containing a lambda.
    // Map literals inside blocks must be on a single line (newlines inside [...] within {...}
    // are statement separators, not whitespace).
    assert_eq!(
        must_succeed(
            "let h = !{spawn { return [ops: [{ |x| return $[$x + 1] }, { |x| return $[$x * 2] }], name: tools] }}\n\
             let r = await $h\n\
             let m = $r[value]\n\
             let inc = $m[ops][0]\n\
             let dbl = $m[ops][1]\n\
             let a = $inc 10\n\
             let bb = $dbl 10\n\
             return $[$a + $bb]"
        ),
        Value::Int(31) // (10+1) + (10*2) = 11 + 20 = 31
    );
}

#[test]
fn spawn_passes_block_as_arg() {
    // Spawn a block that itself spawns — nested concurrency.
    assert_eq!(
        must_succeed(
            r#"
            let h = !{spawn {
                let inner = !{spawn { return 100 }}
                let base = await $inner
                return $[$base[value] + 1]
            }}
            let r = await $h
            return $r[value]
        "#
        ),
        Value::Int(101)
    );
}

#[test]
fn par_many_items() {
    // 50 parallel tasks, each returning a structured value.
    let mut items = Vec::new();
    for i in 0..50 {
        items.push(i.to_string());
    }
    let list = format!("[{}]", items.join(", "));
    let script = format!("!{{par {{ |x| return $[$x * $x] }} {list} 10}}");
    let result = must_succeed(&script);
    if let Value::List(vals) = result {
        assert_eq!(vals.len(), 50);
        assert_eq!(vals[0], Value::Int(0));
        assert_eq!(vals[7], Value::Int(49));
        assert_eq!(vals[49], Value::Int(2401));
    } else {
        panic!("expected List, got {result:?}");
    }
}

#[test]
fn par_returns_closures() {
    // par where each worker returns a value — results cross thread boundaries.
    assert_eq!(
        must_succeed(
            r#"
            let results = !{par { |n| return $[$n * 10] } [2, 3, 5] 3}
            return $[$results[0] + $results[1] + $results[2]]
        "#
        ),
        Value::Int(100) // 20 + 30 + 50
    );
}

#[test]
fn race_first_wins() {
    // Two spawns: one returns immediately, one sleeps. Race picks the fast one.
    assert_eq!(
        must_succeed(
            r#"
            let fast = !{spawn { return winner }}
            let slow = !{spawn { sleep 10; return loser }}
            let r = race [$fast, $slow]
            return $r[value]
        "#
        ),
        Value::String("winner".into())
    );
}

#[test]
fn race_cancelled_await() {
    // After race, awaiting the loser returns an error (cancelled).
    must_fail(
        r#"
        let fast = !{spawn { return ok }}
        let slow = !{spawn { sleep 10; return late }}
        race [$fast, $slow]
        !{await $slow}
    "#,
    );
}

#[test]
fn cancel_makes_await_fail() {
    must_fail(
        r#"
        let h = !{spawn { sleep 1; return ok }}
        cancel $h
        !{await $h}
    "#,
    );
}

#[test]
fn cancel_completed_handle_is_noop() {
    assert_eq!(
        must_succeed(
            r#"
            let h = !{spawn { return 7 }}
            let r = await $h
            cancel $h
            return $r[value]
        "#
        ),
        Value::Int(7)
    );
}

#[test]
fn spawn_error_propagates() {
    // A spawned block that fails — await surfaces the failure.
    must_fail("let h = !{spawn { fail [status: 1] }}\n!{await $h}");
}

#[test]
fn spawn_deep_recursion_in_thread() {
    // TCO works inside spawned threads.
    assert_eq!(
        must_succeed(
            r#"
            let h = !{spawn {
                let count = { |n acc|
                    if $[$n <= 0] { return $acc } else {
                        count $[$n - 1] $[$acc + $n]
                    }
                }
                let total = count 10000 0; return $total
            }}
            let r = await $h
            return $r[value]
        "#
        ),
        Value::Int(50005000)
    );
}

// ── §10.2 guard ─────────────────────────────────────────────────────────

#[test]
fn guard_runs_cleanup_on_success() {
    must_succeed("guard { echo body } { echo cleanup }");
}

#[test]
fn guard_runs_cleanup_on_failure() {
    // Body fails, cleanup still runs, failure propagates.
    must_fail("guard { fail [status: 1] } { echo cleanup }");
}

#[test]
fn guard_propagates_original_error() {
    // The error from body propagates, not from cleanup.
    let result = must_succeed(
        "let r = try { guard { fail [status: 42] } { echo cleanup } } { |e| return $e[status] }\n\
         return $r",
    );
    assert_eq!(result, Value::Int(42));
}

// ── keys, entries, values ───────────────────────────────────────────────

#[test]
fn keys_returns_list() {
    assert_eq!(
        must_succeed("!{keys [a: 1, b: 2, c: 3]}"),
        Value::list(vec![
            Value::String("a".into()),
            Value::String("b".into()),
            Value::String("c".into()),
        ])
    );
}

#[test]
fn keys_empty_map() {
    assert_eq!(must_succeed("!{keys [:]}"), Value::list(vec![]));
}

#[test]
fn entries_returns_pairs() {
    let result = must_succeed("!{entries [x: hello]}");
    if let Value::List(items) = result {
        assert_eq!(items.len(), 1);
        if let Value::List(pair) = &items[0] {
            assert_eq!(pair[0], Value::String("x".into()));
            assert_eq!(pair[1], Value::String("hello".into()));
        } else {
            panic!("expected pair list");
        }
    } else {
        panic!("expected list");
    }
}

#[test]
fn values_returns_values() {
    assert_eq!(
        must_succeed("!{values [a: 1, b: 2]}"),
        Value::list(vec![Value::Int(1), Value::Int(2)])
    );
}

// ── range ───────────────────────────────────────────────────────────────

#[test]
fn range_basic() {
    assert_eq!(
        must_succeed("!{range 1 5}"),
        Value::list(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4)
        ])
    );
}

#[test]
fn range_empty() {
    assert_eq!(must_succeed("!{range 5 5}"), Value::list(vec![]));
}

#[test]
fn range_negative() {
    assert_eq!(
        must_succeed("!{range -2 1}"),
        Value::list(vec![Value::Int(-2), Value::Int(-1), Value::Int(0)])
    );
}

// ── Concurrency: deep nesting and composition ───────────────────────────

#[test]
fn spawn_tree_fan_out_fan_in() {
    // Each level spawns two children, children spawn grandchildren.
    // Leaf nodes return integers; parents sum their children's results.
    // 3 levels deep = 8 leaf nodes, each returning its index.
    assert_eq!(
        must_succeed(
            r#"
            let leaf = { |n| spawn { return $n } }
            let branch = { |a b|
                spawn {
                    let ra = await $a
                    let rb = await $b
                    return $[$ra[value] + $rb[value]]
                }
            }
            let l0 = leaf 1
            let l1 = leaf 2
            let l2 = leaf 3
            let l3 = leaf 4
            let l4 = leaf 5
            let l5 = leaf 6
            let l6 = leaf 7
            let l7 = leaf 8
            let b0 = branch $l0 $l1
            let b1 = branch $l2 $l3
            let b2 = branch $l4 $l5
            let b3 = branch $l6 $l7
            let c0 = branch $b0 $b1
            let c1 = branch $b2 $b3
            let root = branch $c0 $c1
            let r = await $root
            return $r[value]
        "#
        ),
        Value::Int(36) // 1+2+3+4+5+6+7+8
    );
}

#[test]
fn par_of_spawns() {
    // par where each worker itself spawns sub-tasks and awaits them.
    assert_eq!(
        must_succeed(
            r#"
            let work = { |n|
                let a = !{spawn { return $[$n * 10] }}
                let b = !{spawn { return $[$n * 100] }}
                let ra = await $a
                let rb = await $b
                return $[$ra[value] + $rb[value]]
            }
            par $work !{range 1 6} 3
        "#
        ),
        Value::list(vec![
            Value::Int(110), // 10+100
            Value::Int(220), // 20+200
            Value::Int(330),
            Value::Int(440),
            Value::Int(550),
        ])
    );
}

#[test]
fn spawn_pipeline_chain() {
    // Spawn A, await A inside spawn B, await B inside spawn C.
    // Each stage transforms the value. Tests serial dependency across threads.
    assert_eq!(
        must_succeed(
            r#"
            let a = !{spawn { return [1, 2, 3] }}
            let bb = !{spawn {
                let r = await $a
                let items = $r[value]
                let doubled = !{map { |x| return $[$x * 2] } $items}; return $doubled
            }}
            let c = !{spawn {
                let r = await $bb
                let items = $r[value]
                let sum = !{fold { |acc x| return $[$acc + $x] } 0 $items}; return $sum
            }}
            let r = await $c
            return $r[value]
        "#
        ),
        Value::Int(12) // (1*2)+(2*2)+(3*2) = 2+4+6
    );
}

#[test]
fn par_returning_closures_composed() {
    // par produces a list of results, then we fold them.
    assert_eq!(
        must_succeed(
            r#"
            let offsets = !{par { |n| return $n } !{range 1 4} 3}
            return $[$offsets[0] + $offsets[1] + $offsets[2]]
        "#
        ),
        Value::Int(6) // 1+2+3
    );
}

#[test]
fn race_with_spawn_inside() {
    // Each racer itself spawns internal work.
    assert_eq!(
        must_succeed(
            r#"
            let fast = !{spawn {
                let inner = !{spawn { return 42 }}
                let r = await $inner; return $r[value]
            }}
            let slow = !{spawn { sleep 10; return 0 }}
            let r = race [$fast, $slow]
            return $r[value]
        "#
        ),
        Value::Int(42)
    );
}

#[test]
fn spawn_map_reduce() {
    // Classic map-reduce: spawn workers for map phase, reduce results.
    assert_eq!(
        must_succeed(
            r#"
            let items = range 1 11
            let mapped = !{par { |n| return $[$n * $n] } $items 5}
            !{reduce { |a b| return $[$a + $b] } $mapped}
        "#
        ),
        Value::Int(385) // sum of squares 1..10
    );
}

#[test]
fn spawn_passes_closure_that_spawns() {
    // Pass a closure to a spawned thread; that closure itself spawns.
    assert_eq!(
        must_succeed(
            r#"
            let go = { |f n|
                spawn { let out = $f $n; return $out }
            }
            let double_async = { |x|
                let h = !{spawn { return $[$x * 2] }}
                let r = await $h; return $r[value]
            }
            let h = go $double_async 21
            let r = await $h
            return $r[value]
        "#
        ),
        Value::Int(42)
    );
}

// ── unit literal ────────────────────────────────────────────────────────

#[test]
fn unit_literal() {
    assert_eq!(must_succeed("return unit"), Value::Unit);
}

#[test]
fn unit_in_map() {
    assert_eq!(
        must_succeed("let m = [done: unit]\nreturn $m[done]"),
        Value::Unit
    );
}

// ── map pattern defaults ────────────────────────────────────────────────

#[test]
fn map_pattern_default_present_uses_supplied_value() {
    assert_eq!(
        must_succeed(
            "let f = { |m| let [host: h, port: p = 8080] = $m; return $p }\nreturn !{f [host: localhost, port: 8080]}"
        ),
        Value::Int(8080)
    );
}

#[test]
fn map_pattern_default_overridden() {
    assert_eq!(
        must_succeed(
            "let f = { |m| let [host: h, port: p = 8080] = $m; return $p }\nreturn !{f [host: localhost, port: 3000]}"
        ),
        Value::Int(3000)
    );
}

#[test]
fn map_pattern_default_resolves_outer_lexical_name() {
    // The map-pattern default is elaborated at the *pattern's* lexical
    // context, not re-elaborated at assignment time with an empty scope.
    // The default here forces a block that calls `compute-port`, a name
    // bound in the surrounding scope.  Under the old re-elaboration with
    // an empty bindings set, that call resolved to a Head::Bare and was
    // dispatched through the external-command path — `compute-port` was
    // invisible to the elaborator's lexical-scope logic.  With defaults
    // pre-elaborated once at the pattern site, the same name resolves
    // to a Force(Variable(..)) call.
    assert_eq!(
        must_succeed(
            "let compute-port = { return $[8000 + 80] }\n\
             let f = { |m| let [host: h, port: p = !{compute-port}] = $m; return $p }\n\
             return !{f [host: localhost]}"
        ),
        Value::Int(8080)
    );
}

#[test]
fn map_pattern_default_typechecks_with_missing_field() {
    // A pattern entry with a default does not extend the inferred record
    // row, so a caller may omit that key without a typecheck failure.  The
    // default supplies the binding at runtime.  Regression for the row
    // shape decided in typecheck/infer.rs::bind_pattern for Pattern::Map:
    // required entries extend the row, defaulted entries do not.
    assert_eq!(
        must_succeed(
            "let f = { |m| let [host: h, port: p = 8080] = $m; return $p }\nreturn !{f [host: localhost]}"
        ),
        Value::Int(8080)
    );
}

// ── unary negation in arithmetic ────────────────────────────────────────

#[test]
fn arith_unary_negation() {
    assert_eq!(must_succeed("return $[-5]"), Value::Int(-5));
}

#[test]
fn arith_negate_variable() {
    assert_eq!(must_succeed("let xv = 10\nreturn $[-$xv]"), Value::Int(-10));
}

// ── quoted map keys ─────────────────────────────────────────────────────

#[test]
fn quoted_map_key() {
    assert_eq!(
        must_succeed("let m = ['my key': hello]\nreturn $m['my key']"),
        Value::String("hello".into())
    );
}

// ── interpolation type errors ───────────────────────────────────────────

#[test]
fn interpolation_rejects_list() {
    must_fail("let xs = [1, 2]\necho \"items: $xs\"");
}

#[test]
fn interpolation_coerces_int() {
    assert_eq!(
        must_succeed("let n = 42\nreturn \"count: $n\""),
        Value::String("count: 42".into())
    );
}

#[test]
fn interpolation_coerces_unit_to_empty() {
    assert_eq!(
        must_succeed("let uu = unit\nreturn \"val: $uu end\""),
        Value::String("val:  end".into())
    );
}

// ── §8 source circular detection ────────────────────────────────────────

// (circular source requires files; tested via script tests)

// ── §11.4  grant audit: capability-check recording ───────────────────────

fn map_field(v: &Value, key: &str) -> Value {
    match v {
        Value::Map(m) => m.get(key).cloned().unwrap_or(Value::Unit),
        _ => Value::Unit,
    }
}

fn children_of(v: &Value) -> Vec<Value> {
    match map_field(v, "children") {
        Value::List(ch) => ch.into_iter().collect(),
        _ => vec![],
    }
}

fn has_cap_check(children: &[Value], resource: &str, decision: &str) -> bool {
    children.iter().any(|c| {
        let here = map_field(c, "kind") == Value::String("capability-check".into())
            && map_field(c, "resource") == Value::String(resource.into())
            && map_field(c, "decision") == Value::String(decision.into());
        // §11.5 nests grant/within/guard bodies under a scope node, so the
        // capability-check may be a grandchild rather than an immediate child.
        here || has_cap_check(&children_of(c), resource, decision)
    })
}

#[cfg(unix)]
#[test]
fn audit_exec_allowed_recorded() {
    let tree = must_succeed("audit { grant [exec: ['/bin/true': []], audit: true] { /bin/true } }");
    let children = children_of(&tree);
    assert!(
        has_cap_check(&children, "exec", "allowed"),
        "expected allowed exec capability-check in audit tree; children: {:?}",
        children
    );
}

#[cfg(unix)]
#[test]
fn audit_exec_denied_recorded() {
    let tree =
        must_succeed("audit { grant [exec: ['/bin/true': []], audit: true] { /bin/false } }");
    let children = children_of(&tree);
    assert!(
        has_cap_check(&children, "exec", "denied"),
        "expected denied exec capability-check in audit tree; children: {:?}",
        children
    );
}

#[test]
fn audit_no_flag_no_recording() {
    let tree = must_succeed("audit { grant [exec: ['/bin/true': []]] { /bin/true } }");
    let children = children_of(&tree);
    assert!(
        !has_cap_check(&children, "exec", "allowed"),
        "expected no capability-check nodes without audit: true; children: {:?}",
        children
    );
}

#[cfg(unix)]
#[test]
fn audit_nested_grant_outeraudit_propagates() {
    // SPEC §11.5: audit is logical OR — once enabled by an outer grant it
    // stays enabled for nested grants even if they omit audit: true.
    let tree = must_succeed(
        "audit { grant [exec: ['/bin/true': []], audit: true] { grant [exec: ['/bin/true': []]] { /bin/true } } }",
    );
    let children = children_of(&tree);
    assert!(
        has_cap_check(&children, "exec", "allowed"),
        "expected exec event when inner grant lacks audit: true; children: {:?}",
        children
    );
}

// ── §2.4 !{…} hoisting: left-to-right evaluation order ──────────────────

#[test]
fn hoist_multiple_atoms_produce_correct_values() {
    // Two !{…} atoms in one command both evaluate and substitute values.
    let result = must_succeed(
        "let f = { |a b c| return [$a, $b, $c] }\n\
         !{f !{return 1} !{return 2} !{return 3}}",
    );
    assert_eq!(
        result,
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
}

#[cfg(unix)]
#[test]
fn hoist_left_to_right_observable_via_filesystem() {
    // The spec (§2.4) says !{…} atoms in one command are hoisted and
    // evaluated left-to-right, before the containing command runs.
    // Each !{…} here appends a distinct line to a temp file; after the
    // command we read the file and verify the order.
    let path = format!("/tmp/ral_hoist_test_{}.txt", std::process::id());
    let _ = std::fs::remove_file(&path);
    let script = format!(
        "let f = {{ |a b c| return unit }}\n\
         !{{f !{{/bin/sh -c 'echo A >> {path}'}} !{{/bin/sh -c 'echo B >> {path}'}} !{{/bin/sh -c 'echo C >> {path}'}}}}"
    );
    must_succeed(&script);
    let contents = std::fs::read_to_string(&path).expect("temp file exists");
    let _ = std::fs::remove_file(&path);
    assert_eq!(contents, "A\nB\nC\n");
}

#[test]
fn hoist_inside_untaken_chain_arm_does_not_run() {
    // A `?` arm is a conditional context: anything a non-taken arm hoists
    // must not run.  The elaborator isolates each arm's binds
    // (`elab_guarded`), so the `!{fail …}` hoisted inside the fallback is
    // wrapped inside that arm, not outside the chain.  Review F3: the
    // chain used to share the caller's binds accumulator, running the
    // fallback's hoists unconditionally before the chain itself.
    assert_eq!(
        must_succeed("return 1 ? echo !{fail \"untaken arm's hoist ran\"}"),
        Value::Int(1)
    );
}

#[test]
fn earlier_use_of_a_later_non_thunk_let_is_not_shadowed() {
    // Forward declaration covers only the binding shapes group.rs knots
    // (thunk RHS, which can be mutually recursive).  A later `let upper = 5`
    // must not shadow line 1's builtin `upper` into an undefined-variable
    // error.  Review F8: `stmts` used to forward-declare every named let.
    must_succeed("upper hi\nlet upper = 5");
}

#[test]
fn audit_fs_write_denied_recorded() {
    // The grant body evaluates in-process and the redirect is an
    // RAL-owned fs effect, so the fs/denied capability-check fires
    // through `check_fs_op` before the write — no OS sandbox subprocess
    // is involved, so this runs on every host.
    let outside = format!("/nonexistent_ralaudit_test_{}/file.txt", std::process::id());
    let script = format!(
        "audit {{ grant [fs: [write: ['/tmp']], audit: true] {{ to-string 'x' > '{outside}' }} }}"
    );
    let tree = must_succeed(&script);
    let children = children_of(&tree);
    assert!(
        has_cap_check(&children, "fs", "denied"),
        "expected denied fs capability-check in audit tree; children: {:?}",
        children
    );
}

// ── Lexical-audit structural tests (single-audit-path) ───────────────────
//
// One mechanism, one tree shape: every scope-introducing builtin owns
// the audit nodes its body produces.  These tests assert the parent /
// child topology, not just which leaves appear.

/// Find an immediate child of `parent` whose `cmd` matches `name`, or
/// `None`.  The traversal helpers above (`has_cap_check`) recurse; for
/// scope ownership we need to check direct-child placement.
fn child_named<'a>(parent: &'a Value, name: &str) -> Option<&'a Value> {
    match parent {
        Value::Map(m) => match m.get("children") {
            Some(Value::List(items)) => items
                .iter()
                .find(|c| matches!(c, Value::Map(cm) if cm.get("cmd") == Some(&Value::String(name.into())))),
            _ => None,
        },
        _ => None,
    }
}

/// Find an immediate child of `parent` that is a `command` node (any
/// name).  Used when the user only cares that *some* body node ran.
fn first_command_child(parent: &Value) -> Option<&Value> {
    match parent {
        Value::Map(m) => match m.get("children") {
            Some(Value::List(items)) => items
                .iter()
                .find(|c| matches!(c, Value::Map(cm) if cm.get("kind") == Some(&Value::String("command".into())))),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(unix)]
#[test]
fn audit_grant_owns_exec_capability_allowed_child() {
    let tree = must_succeed("audit { grant [exec: ['/bin/true': []], audit: true] { /bin/true } }");
    let outer_children = children_of(&tree);
    let grant = outer_children
        .iter()
        .find(|c| map_field(c, "cmd") == Value::String("grant".into()))
        .expect("audit tree must contain a `grant` scope node");
    let grant_children = children_of(grant);
    assert!(
        has_cap_check(&grant_children, "exec", "allowed"),
        "exec/allowed capability-check must be a child of `grant`, not the root: {:?}",
        grant_children
    );
}

#[cfg(unix)]
#[test]
fn audit_grant_owns_exec_capability_denied_child() {
    // /bin/false exits 1; the exec check is still allowed because the
    // grant lists '/bin/false' implicitly via the prefix.  For an
    // unambiguous "denied" event we issue a grant that does NOT include
    // the requested binary.
    let tree =
        must_succeed("audit { grant [exec: ['/bin/true': []], audit: true] { /bin/false } }");
    let outer_children = children_of(&tree);
    let grant = outer_children
        .iter()
        .find(|c| map_field(c, "cmd") == Value::String("grant".into()))
        .expect("audit tree must contain a `grant` scope node");
    let grant_children = children_of(grant);
    assert!(
        has_cap_check(&grant_children, "exec", "denied"),
        "exec/denied capability-check must be a child of `grant`: {:?}",
        grant_children
    );
}

#[cfg(unix)]
#[test]
fn audit_grant_owns_sandboxed_fs_allowed_child() {
    let tree =
        must_succeed("audit { grant [fs: [read: ['/tmp']], audit: true] { glob '/tmp/*' } }");
    let outer_children = children_of(&tree);
    let grant = outer_children
        .iter()
        .find(|c| map_field(c, "cmd") == Value::String("grant".into()))
        .expect("audit tree must contain a `grant` scope node");
    // The grant body evaluates in-process, so the fs/allowed
    // capability-check for `glob` is recorded directly under the grant
    // scope node.
    let grant_children = children_of(grant);
    assert!(
        has_cap_check(&grant_children, "fs", "allowed"),
        "fs/allowed event must be under `grant`, not loose at the root: outer={:?}",
        outer_children
    );
}

#[test]
fn audit_within_owns_body_children() {
    let tree = must_succeed("audit { within [env: [X: 'y']] { /bin/true } }");
    let outer_children = children_of(&tree);
    let within = outer_children
        .iter()
        .find(|c| map_field(c, "cmd") == Value::String("within".into()))
        .expect("audit tree must contain a `within` scope node");
    assert!(
        first_command_child(within).is_some(),
        "within's body command must be a direct child of the within node: {:?}",
        children_of(within)
    );
}

#[test]
fn audit_guard_owns_body_and_cleanup_children() {
    let tree = must_succeed("audit { guard { /bin/true } { /bin/echo cleaning } }");
    let outer_children = children_of(&tree);
    let guard = outer_children
        .iter()
        .find(|c| map_field(c, "cmd") == Value::String("guard".into()))
        .expect("audit tree must contain a `guard` scope node");
    let guard_children = children_of(guard);
    let cmd_names: Vec<String> = guard_children
        .iter()
        .filter_map(|c| match map_field(c, "cmd") {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect();
    assert!(
        cmd_names.iter().any(|n| n == "/bin/true"),
        "guard body must appear under the guard node: {:?}",
        cmd_names
    );
    assert!(
        cmd_names.iter().any(|n| n == "/bin/echo"),
        "guard cleanup must appear under the guard node: {:?}",
        cmd_names
    );
}

#[test]
fn audit_try_records_try_node_with_body_children() {
    let tree = must_succeed("audit { try { /bin/true } { |_e| return 'caught' } }");
    let outer_children = children_of(&tree);
    let try_node = outer_children
        .iter()
        .find(|c| map_field(c, "cmd") == Value::String("try".into()))
        .expect("audit tree must contain a `try` scope node");
    assert!(
        first_command_child(try_node).is_some(),
        "try's body command must be a child of the try node: {:?}",
        children_of(try_node)
    );
}

#[cfg(unix)]
#[test]
fn audit_nested_grants_produce_nested_grant_nodes() {
    let tree = must_succeed(
        "audit { grant [exec: ['/bin/true': []], audit: true] { grant [exec: ['/bin/true': []]] { /bin/true } } }",
    );
    let outer_children = children_of(&tree);
    let outer_grant = outer_children
        .iter()
        .find(|c| map_field(c, "cmd") == Value::String("grant".into()))
        .expect("outer grant must be present");
    let inner =
        child_named(outer_grant, "grant").expect("inner grant must be a child of the outer grant");
    let _ = inner;
}

#[test]
fn audit_nested_audit_records_one_audit_node_and_returns_it() {
    // Outer `audit { … }` produces the value the test inspects.  The
    // inner `audit { /bin/true }` should appear as exactly one
    // command-shaped child whose `cmd` is "audit", and the inner
    // scope must own the body's command nodes (the recursive
    // ownership property the plan calls out).
    let tree = must_succeed("audit { audit { /bin/true } }");
    let outer_children = children_of(&tree);
    let inner: Vec<_> = outer_children
        .iter()
        .filter(|c| map_field(c, "cmd") == Value::String("audit".into()))
        .collect();
    assert_eq!(
        inner.len(),
        1,
        "exactly one inner `audit` node expected: {:?}",
        outer_children
    );
    // Inner audit owns /bin/true: the body's command node is a child
    // of the inner scope, not a sibling of it in the outer tree.
    let inner_children = children_of(inner[0]);
    let names: Vec<String> = inner_children
        .iter()
        .filter_map(|c| match map_field(c, "cmd") {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect();
    assert!(
        names.iter().any(|n| n.contains("/bin/true")),
        "inner audit's children must include /bin/true: {:?}",
        names
    );
    // And the outer scope must NOT see /bin/true as a direct child —
    // the inner audit absorbs ownership.
    let outer_names: Vec<String> = outer_children
        .iter()
        .filter_map(|c| match map_field(c, "cmd") {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect();
    assert!(
        !outer_names.iter().any(|n| n.contains("/bin/true")),
        "outer audit must not directly own /bin/true: {:?}",
        outer_names
    );
}

#[test]
fn audit_direct_external_pipeline_stage_appears_in_tree() {
    // A `audit { … | … }` body uses pipeline helpers (capture_bytes is
    // off), so the first external stage takes the direct-spawn path.
    // The synthesised command node must still show up.
    let tree = must_succeed("audit { /bin/echo hi | /bin/cat }");
    let cmds: Vec<String> = children_of(&tree)
        .iter()
        .filter_map(|c| match map_field(c, "cmd") {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect();
    assert!(
        cmds.iter().any(|c| c.contains("echo")),
        "direct-spawn echo stage must appear in audit tree: {:?}",
        cmds
    );
}

// ── Process-staged pipeline (surviving M5 helper-protocol paths) ─────────
//
// These four pin the helper-protocol paths the milestone-5 deletion kept
// (the grant-body OS-sandbox re-exec went; the pipeline helper protocol
// stayed).  Each drives a *process-staged* pipeline (at least one byte
// edge) through the public eval path, so the stages run in real
// `--ral-pipeline-stage-helper` / `--ral-bundled-tool` subprocesses
// (the test binary re-execs itself via the ctor in `common/mod.rs`).
// `from-string` is a value consumer with byte input, forcing the pipeline
// to `ProcessStaged` and the RAL stages to `HelperEval`; `wc` / `printf`
// resolve to bundled tools spawned as direct `--ral-bundled-tool`
// children.

/// The pure-pipe equation `x | f = f !{x}` holds across a *process
/// boundary*.  `echo abc | from-string | length` is process-staged: the
/// `from-string` value crosses a value edge into the `length` helper
/// stage, where it is forced once.  `"abc\n"` has four characters, so a
/// correct single force yields 4 — a missing or double force would not.
/// (`scope_escapes::pipeline_non_final_stage_is_not_tail_emitting` pins
/// the same equation for the *in-process* PureValue fold; this is its
/// cross-process counterpart.)
#[cfg(unix)]
#[test]
fn helper_stage_forces_value_edge_across_process_boundary() {
    assert_eq!(
        must_succeed("!{echo abc | from-string | length}"),
        Value::Int(4),
        "the value edge into a helper-eval stage must force exactly once \
         (`x | f = f !{{x}}`): `echo abc` yields `\"abc\\n\"` (4 chars)"
    );
}

/// A helper-eval stage is a subshell: a `cd` inside it must not flow back
/// to the parent.  The stage runs in its own `--ral-pipeline-stage-helper`
/// subprocess, so its cwd change is confined to that process; the parent's
/// logical cwd (`!{pwd}`) must be identical before and after the pipeline.
#[cfg(unix)]
#[test]
fn helper_stage_cd_does_not_flow_back_to_parent() {
    assert_eq!(
        must_succeed(
            "let before = !{pwd}\n\
             !{echo x | from-string | { |_| cd /tmp; return unit }}\n\
             let after = !{pwd}\n\
             return !{equal $before $after}"
        ),
        Value::Bool(true),
        "a `cd` inside a helper-eval pipeline stage must not change the \
         parent's cwd: the stage is an isolated subprocess"
    );
}

/// Audit nodes captured *inside* a helper-eval stage merge back into the
/// parent's audit tree.  The stage runs an external (`/bin/echo`) in its
/// own subprocess; that command's audit node travels back in the stage's
/// `ChildEvalResponse` and must appear somewhere under the surrounding
/// `audit { … }` scope.
#[cfg(unix)]
#[test]
fn helper_stage_audit_nodes_merge_into_parent_tree() {
    fn collect_cmds(v: &Value, out: &mut Vec<String>) {
        if let Value::String(s) = map_field(v, "cmd") {
            out.push(s);
        }
        for child in children_of(v) {
            collect_cmds(&child, out);
        }
    }
    let tree = must_succeed("audit { echo seed | from-string | { |s| /bin/echo $s } }");
    let mut cmds = Vec::new();
    collect_cmds(&tree, &mut cmds);
    assert!(
        cmds.iter().any(|c| c.contains("/bin/echo")),
        "the external run inside a helper-eval stage must merge its audit \
         node into the parent tree: {cmds:?}"
    );
}

/// A bundled byte stage runs as a direct `ral --ral-bundled-tool` child
/// (no helper eval) and produces correct bytes and a success status.
/// `printf … | wc -l` spawns both `printf` and `wc` as direct bundled
/// children; capturing the count through `from-string` recovers `wc`'s
/// bytes (`"4\n"` for four lines) and proves the pipeline succeeded — a
/// non-zero `wc` would fail the `from-string` capture instead.
#[cfg(all(unix, feature = "coreutils"))]
#[test]
fn bundled_byte_stage_runs_as_direct_child_and_produces_bytes() {
    assert_eq!(
        must_succeed("!{printf 'a\\nb\\nc\\nd\\n' | wc -l | from-string}"),
        Value::String("4\n".into()),
        "a bundled `wc -l` over four lines must emit `4`, recovered through \
         the byte-to-value `from-string` edge"
    );
}

/// A failing bundled tool in a process-staged pipeline surfaces a
/// failure rather than swallowing the non-zero status: `wc` on a missing
/// path exits non-zero, and the pipeline must fail-fast.
#[cfg(all(unix, feature = "coreutils"))]
#[test]
fn failing_bundled_byte_stage_surfaces_failure() {
    must_fail("printf 'a\\nb\\n' | wc /nonexistent/path");
}

#[test]
fn audit_captures_stderr_and_caps_at_64kb() {
    // `audit`'s capture policy is `Bytes`; the common finalisation
    // path caps each node's stderr at 64 KB (SPEC §10.3).  Pipe
    // exactly 80 KB of zero-bytes to stderr — bounded source so the
    // tee buffer cannot grow without limit before head closes its
    // input.
    let script = "audit { /bin/sh -c 'head -c 80000 /dev/zero >&2' }";
    let tree = must_succeed(script);
    let children = children_of(&tree);
    let stderr_buf = children.iter().find_map(|c| match map_field(c, "stderr") {
        Value::Bytes(b) if !b.is_empty() => Some(b),
        _ => None,
    });
    // Some build environments lack /bin/sh or head; tolerate that.
    let Some(b) = stderr_buf else {
        return;
    };
    assert!(
        b.len() <= 64 * 1024,
        "captured stderr must be capped at 64 KB; got {} bytes",
        b.len()
    );
}

// ── Regression tests for CHANGELOG items ────────────────────────────────

#[test]
fn first_fails_on_empty_list() {
    // §9.2: first now fails on no-match (including empty input), replacing
    // the old sentinel-unit return.  The failure must be catchable by try.
    must_fail("first { |_| return true } []");
    // When wrapped in try, the failure is turned into an error record.
    let r = must_succeed(
        "let r = try { first { |_| return true } [] } { |e| return 'caught' }\nreturn $r",
    );
    assert_eq!(r, Value::String("caught".into()));
}

#[test]
fn first_returns_match_when_found() {
    let r = must_succeed("first { |x| $[$x > 2] } [1, 2, 3, 4]");
    assert_eq!(r, Value::Int(3));
}

#[test]
fn forward_reference_in_let_group() {
    // §3.1: consecutive lets form an SCC-analysed group; forward references
    // between them must resolve.  `flat-map` in the prelude references
    // `concat` defined later — a regression would show up as "unbound
    // variable" during elaboration.
    let r = must_succeed(
        "let ff = { |x| gg $x }\n\
         let gg = { |x| return $[$x + 1] }\n\
         ff 41",
    );
    assert_eq!(r, Value::Int(42));
}

#[test]
fn forward_reference_in_let_group_block_rhs() {
    // Same SCC-analysed group as `forward_reference_in_let_group`, but the
    // RHS is a parameterless `Ast::Block` rather than `Ast::Lambda`.  Both
    // are thunks (deferred computations), so both must participate in the
    // group's LetRec.  A regression here would re-emit the `b1` binding as
    // a plain `Bind` whose RHS runs eagerly and triggers "unbound a1" before
    // `a1` is in scope.
    let r = must_succeed(
        "let a1 = { return $[!{b1} + 1] }\n\
         let b1 = { return 41 }\n\
         !{a1}",
    );
    assert_eq!(r, Value::Int(42));
}

#[test]
fn flat_map_uses_concat_forward_reference() {
    // The actual prelude case cited in the CHANGELOG: flat-map references
    // concat, which is defined later in the prelude.
    let r = must_succeed("flat-map { |x| return [$x, $x] } [1, 2]");
    assert_eq!(
        r,
        Value::list(vec![
            Value::Int(1),
            Value::Int(1),
            Value::Int(2),
            Value::Int(2),
        ])
    );
}

#[test]
fn hoist_applies_block_and_substitutes() {
    // §2.4: !{$f $x} evaluates $f $x and substitutes its result.
    let r = must_succeed(
        "let double = { |n| return $[$n * 2] }\n\
         let xv = !{double 21}\n\
         return $xv",
    );
    assert_eq!(r, Value::Int(42));
}

// ── expression blocks: logical operators ─────────────────────────────────

#[test]
fn expr_bool_literals() {
    assert_eq!(must_succeed("return $[true]"), Value::Bool(true));
    assert_eq!(must_succeed("return $[false]"), Value::Bool(false));
}

#[test]
fn expr_not_true() {
    assert_eq!(must_succeed("return $[not true]"), Value::Bool(false));
}

#[test]
fn expr_not_false() {
    assert_eq!(must_succeed("return $[not false]"), Value::Bool(true));
}

#[test]
fn expr_not_non_bool_is_error() {
    must_fail("return $[not 1]");
}

#[test]
fn expr_and_both_true() {
    assert_eq!(must_succeed("return $[true && true]"), Value::Bool(true));
}

#[test]
fn expr_and_short_circuits_false_lhs() {
    // `&&` must not evaluate the RHS when the LHS is false; use a
    // force-of-failing-thunk on the RHS to verify laziness.
    let r = must_succeed(
        "let boom = { fail [status: 1] }\n\
         return $[false && !$boom]",
    );
    assert_eq!(r, Value::Bool(false));
}

#[test]
fn expr_or_short_circuits_true_lhs() {
    let r = must_succeed(
        "let boom = { fail [status: 1] }\n\
         return $[true || !$boom]",
    );
    assert_eq!(r, Value::Bool(true));
}

#[test]
fn expr_or_rhs_when_lhs_false() {
    assert_eq!(must_succeed("return $[false || true]"), Value::Bool(true));
}

#[test]
fn expr_mixed_comparisons_and_logic() {
    // `>` binds tighter than `&&` / `||`, so the original parses as
    // `(a > 0) && (a < 10)`.
    let r = must_succeed(
        "let n = 5\n\
         return $[$n > 0 && $n < 10]",
    );
    assert_eq!(r, Value::Bool(true));
}

#[test]
fn expr_precedence_or_below_and() {
    // `a || b && c` must parse as `a || (b && c)`.
    assert_eq!(
        must_succeed("return $[false || true && true]"),
        Value::Bool(true)
    );
    assert_eq!(
        must_succeed("return $[false || true && false]"),
        Value::Bool(false)
    );
}

#[test]
fn expr_non_bool_operand_to_and_is_error() {
    must_fail("return $[1 && true]");
}

// ── Elaborator IR-shape invariants ───────────────────────────────────────

/// `Exec` heads carrying trailing redirects must elaborate to a plain
/// `Exec` with `redirects` populated — `CompKind::Exec` is the only
/// place redirects-on-shell-call live, and the pipeline analyser
/// relies on that exclusivity at `pipeline/analysis.rs` (its External
/// fast path destructures `CompKind::Exec` directly).
#[test]
fn elaborator_never_wraps_exec_in_redirect() {
    use ral_core::ir::CompKind;
    use std::sync::Arc;

    // Each source uses an `Exec` head (bare external name, `^name`,
    // `./path`, `~/path`) with trailing redirects of every flavour
    // (stdout, stderr, append, stdin, fd-dup).  All must elaborate to
    // `Exec` with non-empty `redirects` — never to a `Scope::Redirect`
    // wrapping an `Exec`.
    let sources = &[
        "cat < in.txt",
        "echo hi > out.txt",
        "echo hi >> out.txt",
        "cmd 2> err.txt",
        "cmd 2>&1",
        "^cat < in.txt",
        "./run.sh > out.txt",
        "~/bin/tool 2> err.log",
        "cmd < in.txt > out.txt 2>&1",
    ];

    fn walk(comp: &ral_core::ir::Comp, saw_exec_with_redirects: &mut bool) {
        match &comp.item {
            CompKind::Exec(e) if !e.redirects.is_empty() => {
                *saw_exec_with_redirects = true;
            }
            CompKind::Lam { body, .. } => walk(body, saw_exec_with_redirects),
            CompKind::Bind { comp, rest, .. } => {
                walk(comp, saw_exec_with_redirects);
                walk(rest, saw_exec_with_redirects);
            }
            CompKind::App { head, .. } => walk(head, saw_exec_with_redirects),
            CompKind::Pipeline { stages, .. } => {
                for s in stages {
                    walk(s, saw_exec_with_redirects);
                }
            }
            CompKind::Chain(alts) => {
                for a in alts {
                    walk(a, saw_exec_with_redirects);
                }
            }
            CompKind::Seq(stmts) => {
                for s in stmts {
                    walk(s, saw_exec_with_redirects);
                }
            }
            CompKind::If { then, else_, .. } => {
                walk(then, saw_exec_with_redirects);
                walk(else_, saw_exec_with_redirects);
            }
            _ => {}
        }
    }

    for src in sources {
        let ast =
            ral_core::syntax::parser::parse(src).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
        let comp = Arc::new(ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default()));
        let mut saw = false;
        walk(&comp, &mut saw);
        assert!(
            saw,
            "test bug: {src:?} elaborated without producing an `Exec` \
             with non-empty redirects — the invariant test would pass \
             vacuously.  Adjust the source to exercise the Exec+redirect \
             elaboration path."
        );
    }
}
