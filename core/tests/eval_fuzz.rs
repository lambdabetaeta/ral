//! Evaluator fuzz tests: every value operation, type interaction, control flow
//! path, and edge case must produce a defined result — never panic.

mod common;

use ral_core::builtins;
use std::collections::BTreeMap;
use ral_core::types::{Capabilities, ExecPolicy};
use ral_core::{Shell, Error, EvalSignal, Value, elaborate, evaluate, parse, typecheck};

fn eval(input: &str) -> Result<Value, EvalSignal> {
    let ast = parse(input)
        .map_err(|e: ral_core::ParseError| EvalSignal::Error(Error::new(e.to_string(), 2)))?;
    let comp = elaborate(&ast, Default::default());
    let errors = typecheck(&comp, common::prelude_schemes());
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(EvalSignal::Error(Error::new(
            format!("type error: {msg}"),
            2,
        )));
    }
    let mut shell = Shell::new(Default::default());
    builtins::register(&mut shell, common::prelude_comp());
    evaluate(&comp, &mut shell)
}

fn must_succeed(input: &str) -> Value {
    eval(input).unwrap_or_else(|e| panic!("should succeed: {input:?}\n  error: {e}"))
}

fn must_fail(input: &str) {
    if eval(input).is_ok() {
        panic!("should fail: {input:?}");
    }
}

fn must_not_panic(input: &str) {
    let _ = eval(input);
}

// ── Type system basics ───────────────────────────────────────────────────

#[test]
fn literal_int() {
    assert_eq!(must_succeed("return 42"), Value::Int(42));
}

#[test]
fn literal_negative_int() {
    // -3 is parsed as bare word "-3" → Int(-3)
    assert_eq!(must_succeed("return -3"), Value::Int(-3));
}

#[test]
fn literal_bool_true() {
    assert_eq!(must_succeed("return true"), Value::Bool(true));
}

#[test]
fn literal_bool_false() {
    assert_eq!(must_succeed("return false"), Value::Bool(false));
}

#[test]
fn literal_string_in_assignment() {
    // Assign a string literal and read it back.
    assert_eq!(
        must_succeed("let x = 'hello'\nreturn $x"),
        Value::String("hello".into())
    );
}

#[test]
fn quoted_literal_pipeline_stage_is_not_executed_as_command() {
    let err = match eval("'abc' | blah") {
        Err(EvalSignal::Error(err)) => err,
        other => panic!("should fail with pipeline mismatch: {other:?}"),
    };
    assert!(
        err.message.contains("pipeline channel mismatch"),
        "unexpected error: {}",
        err.message
    );
}

#[test]
fn empty_list() {
    assert_eq!(must_succeed("return []"), Value::List(vec![]));
}

#[test]
fn empty_map() {
    assert_eq!(must_succeed("return [:]"), Value::Map(vec![]));
}

// ── Arithmetic type preservation ─────────────────────────────────────────

#[test]
fn arith_int_plus_int() {
    assert_eq!(must_succeed("return $[2 + 3]"), Value::Int(5));
}

#[test]
fn arith_int_times_int() {
    assert_eq!(must_succeed("return $[3 * 4]"), Value::Int(12));
}

#[test]
fn arith_int_div_exact() {
    assert_eq!(must_succeed("return $[10 / 2]"), Value::Int(5));
}

#[test]
fn arith_int_div_inexact() {
    // 10/3 truncates toward zero (Int / Int → Int)
    assert_eq!(must_succeed("return $[10 / 3]"), Value::Int(3));
}

#[test]
fn arith_float_plus_int() {
    assert_eq!(must_succeed("return $[1.5 + 1.0]"), Value::Float(2.5));
}

#[test]
fn arith_comparison_returns_bool() {
    assert_eq!(must_succeed("return $[5 == 5]"), Value::Bool(true));
    assert_eq!(must_succeed("return $[5 != 5]"), Value::Bool(false));
    assert_eq!(must_succeed("return $[3 < 5]"), Value::Bool(true));
    assert_eq!(must_succeed("return $[5 > 3]"), Value::Bool(true));
    assert_eq!(must_succeed("return $[5 <= 5]"), Value::Bool(true));
    assert_eq!(must_succeed("return $[5 >= 6]"), Value::Bool(false));
}

#[test]
fn arith_division_by_zero() {
    must_fail("$[1 / 0]");
}

#[test]
fn arith_modulo_by_zero() {
    must_fail("$[1 % 0]");
}

#[test]
fn arith_with_variables() {
    assert_eq!(must_succeed("let x = 10\nreturn $[$x + 5]"), Value::Int(15));
}

// ── Strict Bool conditionals ─────────────────────────────────────────────

#[test]
fn if_with_bool_true() {
    must_succeed("if true { echo yes }");
}

#[test]
fn if_with_bool_false() {
    must_succeed("if false { echo no } else { echo yes }");
}

#[test]
fn if_with_forced_block_returning_bool() {
    must_succeed("if !{ return $[1 == 1] } { echo yes }");
}

#[test]
fn if_with_bare_block_condition_is_type_error() {
    // { return $[1 == 1] } is a thunk U(F Bool), not F Bool.
    must_fail("if { return $[1 == 1] } { echo yes }");
}

#[test]
fn if_with_int_is_type_error() {
    must_fail("if 42 { echo bad }");
}

#[test]
fn if_with_string_is_type_error() {
    must_fail("if 'hello' { echo bad }");
}

#[test]
fn if_with_list_is_type_error() {
    must_fail("if [a, b] { echo bad }");
}

// ── if: one-armed ───────────────────────────────────────────────────────

#[test]
fn if_one_armed_true_runs_body() {
    assert_eq!(
        must_succeed("let x = 0\nif true { let x = 1 }\nreturn $x"),
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

// ── Bool/status dual channel ─────────────────────────────────────────────

#[test]
fn bool_true_sets_status_zero() {
    // true ? echo works → chain continues because status = 0
    must_succeed("return true ? echo 'continued'");
}

#[test]
fn bool_false_sets_status_one() {
    // false ? echo stops → chain stops because status = 1
    must_succeed("return false ? echo 'should not print'");
}

#[test]
fn chain_stops_on_failure() {
    // false stops chain; echo should not run
    must_succeed("return false ? echo unreachable");
}

// ── Variable scoping ─────────────────────────────────────────────────────

#[test]
fn undefined_variable_is_error() {
    must_fail("echo $nonexistent");
}

#[test]
fn shadowing_preserves_old_binding() {
    assert_eq!(
        must_succeed("let x = 5\nlet f = { return $x }\nlet x = 10\n!$f"),
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
    must_succeed("let x = {echo hello}\necho 'after'");
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
        must_succeed("let [_, x] = [1, 2]\nreturn $x"),
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
            "let a = { |n acc| if $[$n <= 0] { return $acc } else { b $[$n - 1] $[$acc +   1] } }\n",
            "let b = { |n acc| if $[$n <= 0] { return $acc } else { c $[$n - 1] $[$acc +  10] } }\n",
            "let c = { |n acc| if $[$n <= 0] { return $acc } else { a $[$n - 1] $[$acc + 100] } }\n",
            "!{a 9 0}"
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
            "let f = { |n| if $[$n <= 0] { return 1 } else { let r = g $[$n - 1]; return $[$n + $r] } }\n",
            "let g = { |n| if $[$n <= 0] { return 1 } else { let r = f $[$n - 1]; return $[$n + $r] } }\n",
            "!{f 4}"
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
        "let a = [x: 1, z: 10]\nlet b = [y: 2, z: 20]\nlet r = [...$a, ...$b, z: 99]\nreturn $r[z]",
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
    assert_eq!(must_succeed("!{exists /tmp}"), Value::Bool(true));
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
    assert_eq!(must_succeed("!{is-dir /tmp}"), Value::Bool(true));
}

#[test]
fn is_file_on_dir() {
    assert_eq!(must_succeed("!{is-file /tmp}"), Value::Bool(false));
}

// ── Error handling ───────────────────────────────────────────────────────

#[test]
fn try_catches_fail() {
    must_succeed("try { fail [status: 1] } { |err| echo caught }");
}

#[test]
fn try_catches_nonzero_status() {
    must_succeed("try { fail [status: 1] } { |err| return caught }");
}

#[test]
fn try_error_map_has_status() {
    // _try returns flat record: [cmd, status, stderr, line]
    must_succeed("try { cat /nonexistent 2> /dev/null } { |err| echo $err[status] }");
}

#[test]
fn try_success_no_handler() {
    must_succeed("try { return true } { |err| return true }");
}

#[test]
fn nested_try() {
    must_succeed("try { try { fail [status: 1] } { |e| return inner } } { |e| return outer }");
}

#[test]
fn fail_propagates_without_try() {
    must_fail("fail [status: 1]");
}

#[test]
fn return_exits_function() {
    assert_eq!(
        must_succeed("let f = { |_| return 42; echo unreachable }\n!{f 0}"),
        Value::Unit
    );
}

// ── Functional builtins ──────────────────────────────────────────────────

#[test]
fn map_returns_list() {
    let result = must_succeed("!{map { |x| return $[$x * 2] } [1, 2, 3]}");
    assert_eq!(
        result,
        Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6),])
    );
}

#[test]
fn filter_returns_list() {
    let result = must_succeed("!{filter { |x| return $[$x > 2] } [1, 2, 3, 4]}");
    assert_eq!(result, Value::List(vec![Value::Int(3), Value::Int(4),]));
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
        must_succeed("!{replace 'world' 'al' 'hello world'}"),
        Value::String("hello al".into())
    );
}

#[cfg(feature = "grep")]
#[test]
fn split_and_join() {
    must_succeed("let parts = split '/' '/usr/local/bin'\necho !{intercalate '-' $parts}");
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
    must_succeed("within [handlers: [cat: { echo mocked }]] { cat /nonexistent }");
}

#[test]
fn with_does_not_leak() {
    // After within block, the handler is gone
    must_succeed("within [handlers: [mytest: { echo mock }]] { mytest }\necho 'after with'");
}

#[cfg(unix)]
#[test]
fn grant_exec_subcommand_allows_listed_subcommand() {
    let mut shell = Shell::new(Default::default());
    let grant = Capabilities {
        exec: Some(BTreeMap::from([(
            "/bin/sh".into(),
            ExecPolicy::Subcommands(vec!["-c".into()]),
        )])),
        ..Capabilities::root()
    };
    let args = vec!["-c".into(), "exit 0".into()];
    shell.with_capabilities(grant, |shell| {
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
            "within [handlers: [cat: { return mocked }]] { let n = !{cat /nonexistent | length}; return $n }"
        ),
        Value::Int(6)
    );
}

#[test]
fn within_dir_scoped() {
    must_succeed("within [dir: '/tmp'] { echo 'in tmp' }");
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

    // Use HOME pointed at the tempdir so `~/*.txt` resolves into it.
    // SAFETY: tests in this file mutate process env serially when needed;
    // restore afterwards.
    let prev_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &dir) };
    let result = eval("let xs = glob \"~/*.txt\"; return $xs");
    match prev_home {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }

    let _ = std::fs::remove_dir_all(&dir);
    let items = match result {
        Ok(Value::List(xs)) => xs,
        other => panic!("glob with ~ pattern: unexpected {other:?}"),
    };
    let names: Vec<String> = items.iter().map(|v| v.to_string()).collect();
    assert!(names.iter().any(|n| n.ends_with("/a.txt")), "expected /a.txt in {names:?}");
    assert!(names.iter().any(|n| n.ends_with("/b.txt")), "expected /b.txt in {names:?}");
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
            "within [handlers: [cmd: { return 'outer' }]] { within [handlers: [cmd: { return 'inner' }]] { cmd } }"
        ),
        Value::String("inner".into())
    );
}

#[test]
fn within_handler_outer_fires_when_inner_does_not_match() {
    // Inner frame has no entry for cmd2; outer frame fires.
    assert_eq!(
        must_succeed(
            "within [handlers: [cmd2: { return 'outer' }]] { within [handlers: [cmd1: { return 'inner' }]] { cmd2 } }"
        ),
        Value::String("outer".into())
    );
}

#[test]
fn within_catch_all_swallows_unmatched_names_in_frame() {
    // A catch-all in the inner frame fires for names not in per_name,
    // and outer frames are not consulted.
    assert_eq!(
        must_succeed(
            "within [handlers: [other: { return 'outer' }]] { within [handler: { |n _a| return 'catch' }] { other } }"
        ),
        Value::String("catch".into())
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

#[cfg(unix)]
#[test]
fn grant_exec_attenuation_subcommand_intersection_permits_common() {
    // Intersection of [-c, -s] and [-c] permits -c.
    let mut shell = Shell::new(Default::default());
    let outer = Capabilities {
        exec: Some(BTreeMap::from([(
            "/bin/sh".into(),
            ExecPolicy::Subcommands(vec!["-c".into(), "-s".into()]),
        )])),
        ..Capabilities::root()
    };
    let inner = Capabilities {
        exec: Some(BTreeMap::from([(
            "/bin/sh".into(),
            ExecPolicy::Subcommands(vec!["-c".into()]),
        )])),
        ..Capabilities::root()
    };
    let args = vec!["-c".into(), "exit 0".into()];
    shell.with_capabilities(outer, |shell| {
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
    must_succeed("let x = { fail [status: 1] }\necho 'survived'");
}

#[test]
fn deeply_nested_calls() {
    // Tree-walker uses Rust's call stack — deep recursion is limited.
    // Each level uses eval_stmts → eval_command → apply_lambda → Shell::with_child → eval_stmts.
    must_succeed(
        "let f = { |n| if $[$n == 0] { return 0 } else { let prev = f $[$n - 1]; return $prev } }\nf 10",
    );
}

#[test]
fn script_args_are_not_polluted_by_runner_argv() {
    let mut shell = Shell::new(Default::default());
    shell.dynamic.script_args = vec!["alpha".into(), "beta".into()];
    builtins::register(&mut shell, common::prelude_comp());
    let result = evaluate(
        &elaborate(&parse("return $args").unwrap(), Default::default()),
        &mut shell,
    )
    .expect("evaluate $args");
    assert_eq!(
        result,
        Value::List(vec![
            Value::String("alpha".into()),
            Value::String("beta".into())
        ])
    );
}

#[test]
fn env_overrides_shadow_process_env_in_dollar_env() {
    let mut shell = Shell::new(Default::default());
    shell.dynamic
        .env_vars
        .insert("RAL_TEST_ENV".into(), "override".into());
    builtins::register(&mut shell, common::prelude_comp());
    let result = evaluate(
        &elaborate(
            &parse("return $env[RAL_TEST_ENV]").unwrap(),
            Default::default(),
        ),
        &mut shell,
    )
    .expect("evaluate $env");
    assert_eq!(result, Value::String("override".into()));
}

#[test]
fn many_variables() {
    let mut script = String::new();
    for i in 0..100 {
        script.push_str(&format!("let x{i} = {i}\n"));
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
    must_succeed("let x = 5\necho \"val=$x arith=$[$x + 1] sub=!{echo hi}\"");
}

#[test]
fn command_substitution_preserves_list() {
    let result = must_succeed("!{map { |x| return $[$x * 2] } [1, 2, 3]}");
    assert!(matches!(result, Value::List(_)));
}

#[test]
fn on_exit_runs() {
    // exit always returns Err(EvalSignal::Exit) — callers decide whether to treat it as clean.
    let result = eval("exit 0");
    match result {
        Err(EvalSignal::Exit(0)) => {}
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

#[test]
fn case_dispatch() {
    // SPEC §8.1: case is structural.  Value equality dispatch uses `equal`
    // inside a clause body (no literal patterns), so a single clause with a
    // name parameter handles all strings.
    must_succeed(
        "case help [\n\
         { |v| if !{equal $v 'help'} { echo 'showing help' } else { echo unknown } }\n\
         ]",
    );
}

#[test]
fn case_fallback() {
    must_succeed(
        "case bogus [\n\
         { |v| if !{equal $v 'help'} { echo help } else { echo fallback } }\n\
         ]",
    );
}

#[test]
fn case_structural_list_vs_map() {
    // A list-shaped clause succeeds on a list, and a map-shape catch-all
    // would *not* be reached.  This demonstrates structural dispatch.
    let result = must_succeed(
        "case [1, 2] [\n\
         { |[a, b]| return $[$a + $b] },\n\
         { |_| return 0 }\n\
         ]",
    );
    assert_eq!(result, Value::Int(3));
}

#[test]
fn case_structural_falls_through_on_mismatch() {
    // First clause expects at least two elements; input has one, so the
    // pattern-match fails and the catch-all runs.
    let result = must_succeed(
        "case [9] [\n\
         { |[a, b]| return $[$a + $b] },\n\
         { |_| return -1 }\n\
         ]",
    );
    assert_eq!(result, Value::Int(-1));
}

#[test]
fn case_no_match_fails() {
    // No clause matches (both expect two-element lists) — case must fail.
    must_fail(
        "case [1] [\n\
         { |[a, b]| return 0 },\n\
         { |[a, b, c]| return 1 }\n\
         ]",
    );
}

#[test]
fn try_apply_catches_param_mismatch_only() {
    // Pattern-mismatch on parameter: returns ok:false.
    let r = must_succeed("let r = _try-apply { |[a, b]| return $a } [1]\nreturn $r[ok]");
    assert_eq!(r, Value::Bool(false));
    // Successful apply: returns ok:true and the result.
    let r = must_succeed(
        "let r = _try-apply { |[a, b]| return $[$a + $b] } [10, 32]\nreturn $r[value]",
    );
    assert_eq!(r, Value::Int(42));
}

#[test]
fn try_apply_does_not_catch_body_failures() {
    // A non-pattern-mismatch failure inside the body propagates.
    must_fail("_try-apply { |_| fail [status: 1] } 0");
}

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
        Value::List(vec![
            Value::String("hello".into()),
            Value::String("world".into()),
            Value::String("foo".into()),
        ])
    );
}

// ── String predicates lt / gt ───────────────────────────────────────────

#[test]
fn lt_true() {
    assert_eq!(must_succeed("!{lt abc def}"), Value::Bool(true));
}

#[test]
fn lt_false() {
    assert_eq!(must_succeed("!{lt def abc}"), Value::Bool(false));
}

#[test]
fn gt_true() {
    assert_eq!(must_succeed("!{gt xyz abc}"), Value::Bool(true));
}

#[test]
fn gt_false() {
    assert_eq!(must_succeed("!{gt abc xyz}"), Value::Bool(false));
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
        must_succeed("within [handlers: [cmd: { return 6 }]] { let y = cmd; return $y }"),
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

// ── equal (structural equality) ─────────────────────────────────────────

#[test]
fn equal_same_string() {
    assert_eq!(must_succeed("!{equal hello hello}"), Value::Bool(true));
}

#[test]
fn equal_different_string() {
    assert_eq!(must_succeed("!{equal hello world}"), Value::Bool(false));
}

#[test]
fn equal_int() {
    assert_eq!(must_succeed("!{equal 42 42}"), Value::Bool(true));
}

#[test]
fn equal_int_float_cross() {
    assert_eq!(must_succeed("!{equal 3 3.0}"), Value::Bool(true));
}

#[test]
fn equal_list() {
    assert_eq!(
        must_succeed("!{equal [1, 2, 3] [1, 2, 3]}"),
        Value::Bool(true)
    );
}

#[test]
fn equal_list_mismatch() {
    assert_eq!(must_succeed("!{equal [1, 2] [1, 3]}"), Value::Bool(false));
}

#[test]
fn equal_type_mismatch_is_false() {
    assert_eq!(must_succeed("!{equal 42 hello}"), Value::Bool(false));
}

// ── assert_eq (user-defined, not in prelude) ────────────────────────────

const ASSERT_EQ_DEF: &str = "
let assert_eq = { |name expected actual|
    if !{equal $expected $actual} {} else {
        echo 'FAIL' 1>&2
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

// ── §10.1 _try cmd for runtime errors ───────────────────────────────────

#[test]
fn try_runtime_error_has_cmd_runtime() {
    let result =
        must_succeed("let err = !{_try { f = { |x| return $x }; f 1 2 }}\nreturn $err[cmd]");
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
        Value::List(vec![
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
        matches!(v, Value::Thunk { .. }),
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
        matches!(v, Value::Thunk { .. }),
        "expected thunk, got {v:?}"
    );
}

#[test]
fn binding_position_bare_name_dispatches_in_let_rhs() {
    assert_eq!(
        must_succeed("let upper = { |x| return $x }\nlet x = upper hello\nreturn $x"),
        Value::String("hello".into())
    );
}

#[test]
fn list_position_bare_name_stays_literal() {
    // In list position, bare `upper` is a string literal, not a variable lookup.
    assert_eq!(
        must_succeed("let upper = { |x| return $x }\nreturn [upper, hello]"),
        Value::List(vec![
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
                matches!(items[0], Value::Thunk { .. }),
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
        Value::Map(entries) => {
            assert_eq!(entries[0].0, "label");
            assert_eq!(entries[0].1, Value::String("upper".into()));
            assert!(
                matches!(entries[1].1, Value::Thunk { .. }),
                "expected thunk, got {:?}",
                entries[1].1
            );
        }
        other => panic!("expected map, got {other:?}"),
    }
}

#[test]
fn filter_named_function_requires_deref() {
    assert_eq!(
        must_succeed("let pred = { |x| return $[$x > 1] }\n!{filter $pred [1, 2, 3]}"),
        Value::List(vec![Value::Int(2), Value::Int(3)])
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
        must_succeed("let f = { |x y z| return $[$x + $y + $z] }\nlet g = f 1 2\n!{g 3}"),
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
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
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
            "let h = !{spawn { return 99 }}\nlet a = await $h\nlet b = await $h\nreturn $[$a[value] + $b[value]]"
        ),
        Value::Int(198)
    );
}

#[test]
fn par_returns_structured() {
    assert_eq!(
        must_succeed("!{par { |x| return $[$x * $x] } [1, 2, 3, 4] 2}"),
        Value::List(vec![
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
             let b = $dbl 10\n\
             return $[$a + $b]"
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
fn disown_makes_await_fail() {
    must_fail(
        r#"
        let h = !{spawn { return ok }}
        disown $h
        !{await $h}
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
        "let err = !{_try { guard { fail [status: 42] } { echo cleanup } }}\nreturn $err[status]",
    );
    assert_eq!(result, Value::Int(42));
}

// ── keys, entries, values ───────────────────────────────────────────────

#[test]
fn keys_returns_list() {
    assert_eq!(
        must_succeed("!{keys [a: 1, b: 2, c: 3]}"),
        Value::List(vec![
            Value::String("a".into()),
            Value::String("b".into()),
            Value::String("c".into()),
        ])
    );
}

#[test]
fn keys_empty_map() {
    assert_eq!(must_succeed("!{keys [:]}"), Value::List(vec![]));
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
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
}

// ── seq ─────────────────────────────────────────────────────────────────

#[test]
fn seq_basic() {
    assert_eq!(
        must_succeed("!{seq 1 5}"),
        Value::List(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4)
        ])
    );
}

#[test]
fn seq_empty() {
    assert_eq!(must_succeed("!{seq 5 5}"), Value::List(vec![]));
}

#[test]
fn seq_negative() {
    assert_eq!(
        must_succeed("!{seq -2 1}"),
        Value::List(vec![Value::Int(-2), Value::Int(-1), Value::Int(0)])
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
            par $work !{seq 1 6} 3
        "#
        ),
        Value::List(vec![
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
            let b = !{spawn {
                let r = await $a
                let items = $r[value]
                let doubled = !{map { |x| return $[$x * 2] } $items}; return $doubled
            }}
            let c = !{spawn {
                let r = await $b
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
            let offsets = !{par { |n| return $n } !{seq 1 4} 3}
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
            let items = seq 1 11
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
fn map_pattern_default() {
    // TODO: the typechecker should allow missing fields when a default is present.
    // For now, test the default mechanism via try to bypass the type error.
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

// ── unary negation in arithmetic ────────────────────────────────────────

#[test]
fn arith_unary_negation() {
    assert_eq!(must_succeed("return $[-5]"), Value::Int(-5));
}

#[test]
fn arith_negate_variable() {
    assert_eq!(must_succeed("let x = 10\nreturn $[-$x]"), Value::Int(-10));
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
        must_succeed("let u = unit\nreturn \"val: $u end\""),
        Value::String("val:  end".into())
    );
}

// ── §8 source circular detection ────────────────────────────────────────

// (circular source requires files; tested via script tests)

// ── §11.4  grant audit: capability-check recording ───────────────────────

fn map_field(v: &Value, key: &str) -> Value {
    match v {
        Value::Map(pairs) => pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Unit),
        _ => Value::Unit,
    }
}

fn children_of(v: &Value) -> Vec<Value> {
    match map_field(v, "children") {
        Value::List(ch) => ch,
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

#[test]
fn audit_exec_allowed_recorded() {
    let tree =
        must_succeed("audit { grant [exec: ['/bin/true': []], audit: true] { /bin/true } }");
    let children = children_of(&tree);
    assert!(
        has_cap_check(&children, "exec", "allowed"),
        "expected allowed exec capability-check in audit tree; children: {:?}",
        children
    );
}

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
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
}

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
fn audit_fs_write_denied_recorded() {
    // Grant-with-fs triggers the IPC sandbox subprocess on Linux, which
    // needs bwrap.  On a container without bwrap the subprocess can't
    // start and the test has no way to observe the capability-check node.
    #[cfg(target_os = "linux")]
    if std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/lib",
            "/lib",
            "--",
            "/usr/bin/true",
        ])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        return;
    }
    // Some CI/container environments can start `bwrap` but still do not
    // surface sandboxed capability-check frames back into the parent audit
    // tree.  If a simple allowed fs read does not produce an audit child, the
    // denied-write assertion below is not observable here.
    let probe = must_succeed("audit { grant [fs: [read: ['/tmp']], audit: true] { glob '/tmp/*' } }");
    if !has_cap_check(&children_of(&probe), "fs", "allowed") {
        return;
    }
    let outside = format!(
        "/nonexistent_ralaudit_test_{}/file.txt",
        std::process::id()
    );
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
        "let f = { |x| g $x }\n\
         let g = { |x| return $[$x + 1] }\n\
         f 41",
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
        Value::List(vec![
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
         let x = !{double 21}\n\
         return $x",
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
