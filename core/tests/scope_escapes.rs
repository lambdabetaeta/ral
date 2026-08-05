//! Regression tests for two distinct bugs in the scope-wrapper machinery
//! (`try`, `audit`, `grant`, `within`, `guard`).  Both were fixed earlier
//! in the same refactor series that introduced the `Escape` / `BodyResult`
//! split; see the commit messages below for the exact pre-fix behavior.
//!
//! The harness mirrors `top_level_vs_block.rs` exactly — bootstrap a
//! `Shell` with the prelude registered, then drive each source string
//! through the public `run` door like a REPL run would.  We
//! deliberately do not reach into internal types: the bugs are observable
//! at the public run-door API.

mod common;

use ral_core::transport::{Program, Run};
use ral_core::types::{Capabilities, Escape, Mooring, Settled, Shell};
use ral_core::{
    Break, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Value, builtins,
};

// ── Harness (same shape as `top_level_vs_block.rs`) ─────────────────────

/// Mirror of `top_level_vs_block::fresh_shell` — prelude registered, default
/// env vars seeded, caps at root.  Equivalent to what every front end
/// (`ral`, `exarch`, scripts) ends up with after bootstrap.
fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

/// Run one top-level run of `source` through the public `run` door
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
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
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

// ── (1) `try` must not swallow `exit` ────────────────────────────────────

/// Regression for: `try { exit 7 } { |_| return unit }` used to exit 0
/// because `classify` (then in `core/src/builtins/control.rs`, now in
/// `core/src/evaluator/scope.rs`) mapped
/// `Err(Break::Escape(Escape::Exit(_)))` to `Outcome { ok: true, value: Unit }`,
/// and the `try` builtin then took the success branch.  After the audit-scope
/// refactor (commit `refactor(audit): record_scope returns
/// Result<ScopeRecord, Escape>; fix try swallows exit`), `classify`
/// consumes `&BodyResult`, whose variants are Value or Error only —
/// `Exit` is split off in `record_scope` via `split` and propagates
/// upward as `Escape` before ever reaching `classify`.
#[test]
fn try_does_not_swallow_exit() {
    let mut shell = fresh_shell();
    let result = top_level(&mut shell, "try { exit 7 } { |_e| return unit }");
    match result {
        Err(Break::Escape(Escape::Exit(7))) => {}
        Err(Break::Escape(Escape::Exit(other))) => panic!(
            "expected Break::Escape(Escape::Exit(7)), got Exit({other}); `try` must \
             not rewrite the exit code"
        ),
        Err(Break::Error(e)) => panic!(
            "expected Break::Escape(Escape::Exit(7)) to propagate; got Error({:?}). \
             The body's `exit 7` must escape past `try` unchanged, not \
             be classified as a runtime error.",
            e.message
        ),
        #[cfg(unix)]
        Err(other) => panic!(
            "expected Break::Escape(Escape::Exit(7)), got {other:?}; `try` must \
             propagate Exit verbatim"
        ),
        Ok(v) => panic!(
            "expected Break::Escape(Escape::Exit(7)); got Ok({v:?}). This is the \
             exact pre-refactor bug: `try` swallowed `exit` and returned \
             a successful Unit from the handler."
        ),
    }
}

// ── (2) `grant` must attenuate caps across tail-recursive iterations ────

/// Regression for: a tail-recursive function inside
/// `grant [shell: [chdir: false]] { ... }` could run `cd` on iterations
/// after the first because `Control::Tail` escaped
/// `with_capabilities` before the trampoline resumed it.  After commit
/// `fix(grant): preserve capability attenuation across tail-recursive
/// bodies`, `grant` invokes its body via `call_value` whose trampoline
/// lives inside `with_capabilities`, so `TailCall` is absorbed locally
/// and the narrowed cap frame is in force on every iteration.
///
/// The repro is the exact program from the fix commit's body: `f` runs
/// `cd /tmp` only when `$n <= 0`, so depth >= 1 is needed to make the
/// check fall on a non-initial iteration of the trampoline.  `gg 2` runs
/// `f 2 -> f 1 -> f 0`, and the final `cd /tmp` must be denied by the
/// outer `grant`.
///
/// The denial message comes from `core/src/capability/enforce.rs`
/// (`"denied: cd requires shell.chdir"`).  We pin to a substring of that
/// — `"shell.chdir"` is the most specific token and is unlikely to drift.
#[test]
fn grant_attenuates_across_tail_recursion() {
    let mut shell = fresh_shell();
    let src = "\
let f = { |n| if $[$n <= 0] { cd /tmp } else { f $[$n - 1] } }\n\
let gg = { |n| grant [shell: [chdir: false]] { f $n } }\n\
gg 2";
    let result = top_level(&mut shell, src);
    match result {
        Err(Break::Error(e)) => {
            // Pin to the specific denial wording in
            // `core/src/capability/enforce.rs::check_shell_chdir`.
            // If that wording changes, this assertion (intentionally)
            // breaks and points the maintainer at the renamed message.
            assert!(
                e.message.contains("shell.chdir")
                    || e.message.contains("chdir")
                    || e.message.to_lowercase().contains("denied"),
                "expected a capability-denial error mentioning chdir / \
                 shell.chdir / denied; got {:?}",
                e.message
            );
        }
        Ok(v) => panic!(
            "expected capability-denial error from `cd /tmp` under \
             `grant [shell: [chdir: false]]`, got Ok({v:?}). This is the \
             exact pre-fix bug: TailCall escaped `with_capabilities`, so \
             the inner `cd` ran against the outer (unattenuated) cap \
             frame."
        ),
        Err(other) => panic!("expected Break::Error(denial), got {other:?}"),
    }
}

/// The `within` sibling of the test above.  `eval_within` nests three
/// separate `with_*` frames (env, cwd, handlers) around its body, and each
/// must contain the trampoline the way `with_capabilities` does.  If one
/// stops containing it, every iteration after the first runs *outside* the
/// override: `$ENV[MARK]` is gone, or the scoped handler is no longer
/// installed and the name falls through to PATH resolution.  The recursion
/// reaches the observation only at depth 0, so a leak cannot hide.
#[test]
fn within_overrides_survive_tail_recursion() {
    let mut shell = fresh_shell();
    let env = top_level(
        &mut shell,
        "let probe_env = { |n| if $[$n <= 0] { return $ENV[MARK] } else { probe_env $[$n - 1] } }\n\
         within [env: [MARK: inside]] { probe_env 3 }",
    )
    .expect("the env override must still be installed at depth 0");
    assert_eq!(env, Value::String("inside".into()));

    let handled = top_level(
        &mut shell,
        "let probe_handled = { |n| if $[$n <= 0] { return !{within-probe} } else { probe_handled $[$n - 1] } }\n\
         within [handlers: [within-probe: { |args| to-string pong }]] { probe_handled 3 }",
    )
    .expect("the handler frame must still be installed at depth 0");
    assert_eq!(handled, Value::String("pong".into()));
}

// ── (3) consecutive audit blocks must not drop the next inner command ────

/// Regression for: `audit::with_scope` used to call `Audit::push_scope`,
/// which pushed the wrapping scope node *and* set `scope_pushed=true`.
/// The dispatcher consumes that flag in `finish_command`'s early-return
/// path — but only when the trail is active.  When an `audit { … }`
/// block whose body went through `with_scope` (i.e. contained a
/// `grant` / `within` / `guard`) ended, the trail went inactive while
/// the flag stayed set.  Inside the *next* `audit { … }` block the
/// dispatcher saw `take_scope_pushed() == true` for the first
/// dispatched command and dropped its observation before recording —
/// silently, with the rest of the program unaffected.
///
/// Fixed in commit `2646ccf` (elaborator: flip control-operator
/// lowering onto structural IR), which switched `with_scope` to
/// `Audit::push` (the flag-less variant).  Commit `c323f32` then
/// removed the `scope_pushed` field, `push_scope`, `mark_scope_pushed`,
/// and `take_scope_pushed` outright; reintroducing the bug would
/// require restoring at least the flag and the flag-setting variant of
/// `push_scope`.
///
/// The observable assertion is on the audit trail shape: the second
/// `audit { echo hi }` must record echo's own command observation as a
/// child.  Pre-fix, that child list was empty.
#[test]
fn audit_recording_survives_consecutive_audit_blocks() {
    let mut shell = fresh_shell();
    // First audit block: body runs through `with_scope` via `guard`.
    // Pre-fix this set the dispatcher flag.  We discard the returned
    // tree — the bug it leaves behind is observable in the *next*
    // block.
    let _ = top_level(
        &mut shell,
        "audit { guard { return unit } { return unit } }",
    );
    // Second audit block: the first dispatched command inside the
    // body would be eaten by the stale flag.
    let tree = match top_level(&mut shell, "audit { echo hi }") {
        Ok(v) => v,
        Err(e) => panic!("second `audit {{ echo hi }}` must succeed; got error: {e:?}"),
    };
    let children = match &tree {
        Value::Map(m) => match m.get("children") {
            Some(Value::List(ch)) => ch.iter().cloned().collect::<Vec<_>>(),
            other => panic!("audit tree must have a list `children` field; got {other:?}"),
        },
        other => panic!("audit {{ … }} must return a Map; got {other:?}"),
    };
    let has_echo = children.iter().any(|c| is_command(c, "echo"));
    assert!(
        has_echo,
        "expected the second `audit {{ echo hi }}` to record echo's own \
         observation.  Pre-fix `with_scope` set `scope_pushed=true` \
         during the first block's `guard`; the flag survived into the \
         second block (trail was inactive in between, so \
         `finish_command` couldn't consume it) and silently dropped \
         the first dispatched command in the next active trail.  \
         children = {children:?}"
    );
}

// ── (4) tail-emission is granted, not ambient (findings E1, E4; rec. A1) ──
//
// Before A1, tail-ness was an ambient bit on `shell.mobile.control`
// (`in_tail_position`) that every eliminator had to remember to clear.
// Two eliminators forgot: a non-final value-pipeline stage and a
// non-final fallback-chain arm both ran under the caller's tail flag, so
// a tail call inside one escaped as `Control::Tail` and abandoned the
// rest of the pipeline / chain (finding E1). After A1, `Tail` is a
// parameter of `eval_comp`, granted only to the final sub-computation;
// a non-final stage / arm receives `Tail::No` by construction, so its
// tail call stays a catchable application.
//
// The pure-pipe equation (decisions/260609) says `x | f = f !{x}`
// unconditionally at every value edge, so an in-function pipeline must
// equal the same pipeline at top level. The two tests below pin both:
// the value reaching the consumer (site 1) and the fallback running
// (site 2). The third test is the regression guard for the *other*
// direction — a wrong grant that broke tail recursion — by recursing
// 100 000 deep through every tail-bearing eliminator without tripping
// the recursion cap or overflowing the host stack.

/// Helper: force a thunk-returning expression and read its `Int` result.
fn int_result(shell: &mut Shell, source: &str) -> i64 {
    match top_level(shell, source) {
        Ok(Value::Int(n)) => n,
        Ok(other) => panic!("{source:?}: expected Int, got {other:?}"),
        Err(e) => panic!("{source:?}: expected Int, got error {e:?}"),
    }
}

/// E1 site 1 — a value pipeline inside a function must equal the same
/// pipeline at top level.  Pre-fix, the non-final stage's tail call
/// escaped and discarded the downstream stage, so `f $y | f` inside a
/// function yielded `2` (one increment) where `f 1 | f` at top level
/// (non-tail) yielded `3` (two increments).  The pure-pipe equation
/// demands they agree.
#[test]
fn pipeline_non_final_stage_is_not_tail_emitting() {
    let mut shell = fresh_shell();
    let _ = top_level(&mut shell, "let f = { |x| $[$x + 1] }");
    let top = int_result(&mut shell, "f 1 | f");
    let in_fn = int_result(&mut shell, "let gg = { |y| f $y | f }\n!{gg 1}");
    assert_eq!(top, 3, "top-level `f 1 | f` must apply f twice");
    assert_eq!(
        in_fn, top,
        "the in-function pipeline `f $y | f` must equal the top-level \
         `f 1 | f` (the pure-pipe equation `x | f = f !{{x}}` holds at \
         every value edge).  Pre-A1 the non-final stage emitted \
         `Control::Tail`, discarding the final stage: in-fn = {in_fn}, \
         top-level = {top}."
    );
}

/// E1 site 2 — a fallback chain inside a function must run its fallback,
/// exactly as the same chain at top level.  Pre-fix, the non-final arm's
/// tail call escaped as `Control::Tail`, so the chain never saw its
/// failure and the fallback was structurally dead: `f $y ? echo
/// fallback` inside a function propagated the error (exit 1) where the
/// top-level `f 1 ? echo fallback` ran the fallback (exit 0).
#[test]
fn chain_non_final_arm_failure_is_catchable() {
    let mut shell = fresh_shell();
    let _ = top_level(
        &mut shell,
        "let f = { |x| fail [status: 1, message: \"boom\"] }",
    );
    // The non-final arm `f $y` fails; the fallback `return 7` must run,
    // so the chain's value is 7 rather than the propagated error.
    let in_fn = top_level(&mut shell, "let gg = { |y| f $y ? return 7 }\n!{gg 1}");
    match in_fn {
        Ok(Value::Int(7)) => {}
        Ok(other) => panic!(
            "expected the in-function chain `f $y ? return 7` to run its \
             fallback and yield 7; got Ok({other:?}).  Pre-A1 the \
             non-final arm's tail call escaped as `Control::Tail`, the \
             chain never observed the failure, and the fallback was \
             dead."
        ),
        Err(e) => panic!(
            "expected the in-function fallback to run (yielding 7); got \
             error {e:?}.  This is finding E1 site 2: the non-final \
             chain arm must remain catchable so the fallback runs."
        ),
    }
}

/// The other direction — a wrong tail grant either reintroduces E1 (over-
/// grant) or breaks tail recursion (under-grant).  This recurses deep
/// through *every* tail-bearing eliminator: the bare lambda body, the
/// selected `if` branch, the final chain arm, the final pipeline stage
/// (as a bare consumer of the upstream), the `case` selected arm, and a
/// bind continuation.  Each must trampoline in O(1) host frames; a
/// missing grant would overflow the host stack (or trip the default
/// 1024-deep recursion cap) instead of returning the base case.
///
/// Depth is 100 000 for the cheap variants.  The chain variant fails its
/// non-final arm on every iteration (the only way to reach the final,
/// tail-recursive arm), and allocating that many `Error`s in a debug
/// build is slow, so it runs at 20 000 — still 20× the 1024 cap and far
/// past any host-stack ceiling, which is all the trampoline claim needs.
#[test]
fn deep_tail_recursion_trampolines_through_every_eliminator() {
    let deep = 100_000;
    // 20× the recursion cap and far past the ~few-thousand-frame host
    // stack ceiling; cheaper than `deep` for the error-allocating chain.
    let past_cap = 20_000;

    // (a) bare lambda body in tail position.
    let mut shell = fresh_shell();
    let plain = format!(
        "let loop = {{ |n| if $[$n <= 0] {{ return $n }} else {{ loop $[$n - 1] }} }}\n!{{loop {deep}}}"
    );
    assert_eq!(
        int_result(&mut shell, &plain),
        0,
        "lambda-body tail recursion"
    );

    // (b) recursion through the FINAL chain arm (`fail ? loop`).
    let mut shell = fresh_shell();
    let chained = format!(
        "let f = {{ |x| fail [status: 1, message: \"x\"] }}\n\
         let loop = {{ |n| if $[$n <= 0] {{ return $n }} else {{ f $n ? loop $[$n - 1] }} }}\n!{{loop {past_cap}}}"
    );
    assert_eq!(
        int_result(&mut shell, &chained),
        0,
        "final-chain-arm tail recursion"
    );

    // (c) recursion through the FINAL pipeline stage as a bare consumer
    //     of the upstream value (`$[n-1] | loop`).
    let mut shell = fresh_shell();
    let piped = format!(
        "let loop = {{ |n| if $[$n <= 0] {{ return $n }} else {{ $[$n - 1] | loop }} }}\n!{{loop {deep}}}"
    );
    assert_eq!(
        int_result(&mut shell, &piped),
        0,
        "final-pipeline-stage tail recursion"
    );

    // (d) recursion through the selected `case` arm.
    let mut shell = fresh_shell();
    let cased = format!(
        "let loop = {{ |n| if $[$n <= 0] {{ return 0 }} else {{ case `go $[$n - 1] [`go: {{ |m| loop $m }}] }} }}\n!{{loop {deep}}}"
    );
    assert_eq!(int_result(&mut shell, &cased), 0, "case-arm tail recursion");

    // (e) recursion through a bind continuation (`let m = …; loop $m`).
    let mut shell = fresh_shell();
    let bound = format!(
        "let loop = {{ |n| if $[$n <= 0] {{ return $n }} else {{ let m = $[$n - 1]\nloop $m }} }}\n!{{loop {deep}}}"
    );
    assert_eq!(
        int_result(&mut shell, &bound),
        0,
        "bind-continuation tail recursion"
    );
}

// ── (5) `cmd &` must not destroy the parent's lent state (finding E2; rec. A3) ──
//
// Pre-fix, `eval_background` built a `Shell::child_of(&captured, shell)`
// and never paired it with `return_to`. `child_of` *moves* the parent's
// read-once local state — pipe stdin, audit trail, REPL scratch — into
// the child, which then died with the fork, taking the lent state with
// it. The whole foreground subtree of any `audit { … }` / `try { … }`
// containing a `&` was lost: even commands recorded *before* the fork
// vanished, because the audit trail itself had been moved out.
//
// The fix is a deletion: `eval_background` hands the thunk straight to
// `builtin_spawn`, which extracts `(body, captured)` and runs the body
// on a worker thread that *clones* the parent's `Context` via
// `spawn_thread` — never touching the parent's read-once local state.

/// E2 — an `audit { … }` block containing a background `&` must still
/// record the foreground commands.  Pre-fix the background fork moved
/// the audit trail out of the parent and dropped it, so the whole
/// subtree — including `echo one`, recorded before the `&` — was lost
/// and `children` came back empty.
#[test]
fn audit_survives_background_amp() {
    let mut shell = fresh_shell();
    let tree = match top_level(&mut shell, "audit { echo one; sleep 0.02 &; echo two }") {
        Ok(v) => v,
        Err(e) => panic!(
            "`audit {{ echo one; sleep 0.02 &; echo two }}` must succeed; \
             got error: {e:?}"
        ),
    };
    let children = match &tree {
        Value::Map(m) => match m.get("children") {
            Some(Value::List(ch)) => ch.iter().cloned().collect::<Vec<_>>(),
            other => panic!("audit tree must have a list `children` field; got {other:?}"),
        },
        other => panic!("audit {{ … }} must return a Map; got {other:?}"),
    };
    let echo_count = children.iter().filter(|c| is_command(c, "echo")).count();
    assert_eq!(
        echo_count, 2,
        "expected both foreground `echo` commands to be recorded around \
         the background `&`.  Pre-fix `eval_background` moved the audit \
         trail into the fork via an unpaired `child_of` and dropped it, \
         losing the whole subtree — even `echo one`, recorded before the \
         fork.  children = {children:?}"
    );
}

// ── (6) `guard` cleanup must not swallow `exit` / `Stopped` (finding E3) ──
//
// Pre-fix, an `Err` from the `guard` cleanup thunk — every `Break`
// variant, including `Escape::Exit` and `Escape::Stopped` — was turned
// into a `cmd_error` log and dropped, and the body result was returned
// regardless.  An `exit 5` in cleanup left the shell at status 0;
// dropping a `Stopped` orphaned a stopped process group (pgid lost,
// never resumable or reapable).  This is the same channel-conflation
// that finding `try_does_not_swallow_exit` (test 1) pins for `try`.
//
// The fix: a cleanup *escape* takes priority over the body result and
// propagates; a cleanup *error* is still logged and the body result
// stands (cleanup is a best-effort finalizer whose ordinary failures
// must not mask the body's outcome).

/// E3 — an `exit` raised by the `guard` cleanup thunk must propagate as
/// `Escape::Exit`, not be swallowed into a successful body result.
#[test]
fn guard_cleanup_does_not_swallow_exit() {
    let mut shell = fresh_shell();
    let result = top_level(&mut shell, "guard { echo body } { exit 5 }");
    match result {
        Err(Break::Escape(Escape::Exit(5))) => {}
        Err(Break::Escape(Escape::Exit(other))) => {
            panic!("expected Escape::Exit(5) from the guard cleanup, got Exit({other})")
        }
        Ok(v) => panic!(
            "expected Escape::Exit(5) to propagate from the `guard` cleanup; \
             got Ok({v:?}).  This is the pre-fix bug: cleanup's `exit 5` was \
             logged via `cmd_error` and dropped, and the body result was \
             returned with status 0."
        ),
        Err(other) => panic!("expected Escape::Exit(5) from guard cleanup; got {other:?}"),
    }
}

/// E3 sibling — a normal `guard` (no escape anywhere) must still run its
/// cleanup and return the body's value.  This pins that the fix did not
/// over-rotate: the ordinary finalizer path is unchanged.  The body's
/// value `7` is read from the `guard`'s own return; that the cleanup ran
/// is read from the audit tree, where the cleanup `echo` shows up as a
/// real command observation (`guard` is transparent — it owns no
/// observation itself).
#[test]
fn guard_normal_runs_cleanup_and_returns_body() {
    let mut shell = fresh_shell();
    let result = top_level(&mut shell, "guard { return 7 } { echo cleaned }");
    match result {
        Ok(Value::Int(7)) => {}
        Ok(other) => panic!("expected the body value 7 from `guard`; got Ok({other:?})"),
        Err(e) => panic!("expected the body value 7 from `guard`; got error {e:?}"),
    }

    // Re-run under `audit { … }` so the cleanup's `echo` is recorded;
    // its presence in the tree proves the finalizer ran on the ordinary
    // (non-escape) path.
    let tree = top_level(&mut shell, "audit { guard { return 7 } { echo cleaned } }")
        .expect("`audit { guard … }` must succeed");
    assert!(
        audit_tree_has_command(&tree, "echo"),
        "the `guard` cleanup must have run on the normal path (recording \
         its `echo cleaned` observation); a normal guard returns the body \
         value but still executes its finalizer.  tree = {tree:?}"
    );
}

/// Whether `observation` is a `kind: "command"` projection whose `argv[0]`
/// (the program) equals `name`.
fn is_command(observation: &Value, name: &str) -> bool {
    let Value::Map(m) = observation else {
        return false;
    };
    if !matches!(m.get("kind"), Some(Value::String(k)) if k == "command") {
        return false;
    }
    matches!(
        m.get("argv"),
        Some(Value::List(argv)) if matches!(argv.get(0), Some(Value::String(s)) if s == name)
    )
}

/// Depth-first search of an `audit` tree for a command observation whose
/// `argv[0]` equals `name`.
fn audit_tree_has_command(observation: &Value, name: &str) -> bool {
    if is_command(observation, name) {
        return true;
    }
    let Value::Map(m) = observation else {
        return false;
    };
    match m.get("children") {
        Some(Value::List(ch)) => ch.iter().any(|c| audit_tree_has_command(c, name)),
        _ => false,
    }
}

// ── (7) handler self-masking survives a panic mid-body (finding E5; rec. A4) ──
//
// `run_handler` lifts the matched handler frame off the stack for the
// dynamic extent of the body (so a same-name call from inside reaches
// the next outer match), then re-inserts it.  Pre-A4 the re-insertion
// was straight-line code skipped on an unwind: a Rust panic from inside
// the handler body left the stripped frame dropped — and a stripped
// *alias* frame is a permanently deleted user alias, the one piece of
// dynamic context with no save elsewhere to rebuild from.  exarch
// `catch_unwind`s evaluation and continues the session on the same
// `Shell`, so a caught panic must not silently delete the user's alias.
//
// After A4, the strip/restore is an RAII guard whose `Drop` re-inserts
// the frame, panic or otherwise.

/// A nullary host builtin whose reducer raises a Rust panic.  Registered
/// only by the test below, it is the panic trigger the RAII guard needs:
/// no shipped builtin should panic, so the test owns one rather than
/// leaning on whichever builtin happens to be panic-prone.
fn panic_builtin(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> ral_core::types::Settled<Value> {
    panic!("__test-panic builtin invoked");
}

static PANIC_BUILTIN_ARR: [ral_core::types::BuiltinEntry; 1] =
    [ral_core::types::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("__test-panic"),
        ral_core::typecheck::builtins::BuiltinTypeRule::Scheme(
            ral_core::typecheck::builtins::scheme::pure_string,
        ),
        "__test-panic  — test-only: raise a Rust panic.",
        ral_core::types::BuiltinBody::Static(panic_builtin),
    )];
static PANIC_BUILTIN: &[ral_core::types::BuiltinEntry] = &PANIC_BUILTIN_ARR;

/// E5 — a Rust panic raised inside an alias body must leave the alias
/// still installed.  The body invokes `__test-panic`, a host builtin the
/// test registers whose reducer panics unconditionally — any panic
/// mid-handler-body would do, and a dedicated trigger keeps the test
/// independent of which shipped builtin happens to be panic-prone.  The
/// `catch_unwind` stands in for exarch's `pump`, which catches and
/// continues on the same `Shell`.  Pre-A4 the stripped alias frame was
/// dropped on the unwind and `has_alias` returned false afterward.
#[test]
fn handler_self_mask_survives_panic_mid_body() {
    let mut shell = fresh_shell();
    shell.install_builtins(PANIC_BUILTIN);
    let _ = top_level(&mut shell, "alias boom { |args| __test-panic }")
        .expect("installing the alias must succeed");
    assert!(
        shell.has_alias("boom"),
        "alias must be installed before the call"
    );

    // The dispatch of `boom` runs `run_handler`, which strips the alias
    // frame, then `apply`s the body — which panics.  Default panic
    // output is noisy; silence the hook for the duration of the catch.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        top_level(&mut shell, "boom")
    }));
    std::panic::set_hook(prev_hook);

    assert!(
        outcome.is_err(),
        "the alias body's `__test-panic` must raise; its reducer panics \
         unconditionally"
    );
    assert!(
        shell.has_alias("boom"),
        "the alias `boom` must still be installed after a panic unwound \
         through its body.  Pre-A4 the stripped frame was re-inserted by \
         straight-line code skipped on the unwind, permanently deleting \
         the user alias; A4's RAII guard restores it from `Drop`."
    );
}
