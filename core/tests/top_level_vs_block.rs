#![allow(clippy::disallowed_methods)]

//! Semantics of the top-level vs block contract: what persists at the
//! top level, what is discarded across block-shaped scopes (`grant`,
//! `within`, `try`, `guard`, `audit`), and what stays consistent between
//! local and sandbox-confined transport.
//!
//! These tests exercise the **documented** semantics of the top-level
//! run and the block contract beneath it.  They drive the public
//! `run` door the same way a REPL run (`ral`), a tool call
//! (`exarch`), or a forced thunk inside a `grant { ... }` body would —
//! never reach into internal types.
//!
//! The two-call harness mirrors what exarch's `shell_eval::run_shell`
//! does between consecutive tool calls and what the ral REPL's
//! `execute_input` does between consecutive prompt runs: hold a single
//! [`Shell`] across calls and route each body through the public
//! `run` door, which checks against the live session before running.

mod common;

use ral_core::transport::{Program, Run};
#[cfg(unix)]
use ral_core::types::FsPolicy;
use ral_core::types::{Capabilities, Settled, Shell};
use ral_core::{Break, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Value};

// ── Harness ─────────────────────────────────────────────────────────────

/// Build a `Shell` that mirrors what every front end (`ral`, `exarch`,
/// scripts) ends up with after its bootstrap: prelude registered,
/// default env vars seeded, capabilities at root.  Equivalent to
/// `exarch::bootstrap::boot_shell()` without the TUI / signal-handler
/// pieces, which are irrelevant to boundary semantics.
fn fresh_shell() -> Shell {
    ral_core::boot::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::boot::HostSurface::default(),
    )
}

/// Run one top-level run against `shell` through the public `run`
/// door, matching exarch's per-tool flow: the door checks `source`
/// against the live env, then evaluates it.  Returns whatever the body
/// returned (or the body's error).  Every test below picks source it
/// expects to compile, so a static diagnostic is a test bug.
fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    top_level_under_request(shell, Capabilities::root(), source)
}

/// Run one top-level run under an attenuated `Capabilities` frame,
/// carried into the `run` door's [`RunRequest`] exactly as exarch
/// attenuates its tool runs.  Used by the sandbox-parity tests below.
#[cfg(unix)]
fn top_level_under(shell: &mut Shell, caps: Capabilities, source: &str) -> Settled<Value> {
    top_level_under_request(shell, caps, source)
}

/// Shared body of [`top_level`] and [`top_level_under`]: drive the public
/// `run` door under `caps` and return the body's `Settled<Value>`.
fn top_level_under_request(shell: &mut Shell, caps: Capabilities, source: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps,
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
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Capability frame that actually triggers `sandbox_projection()` to
/// return `Some(_)`: any `fs` policy is enough.  Read prefix `/` makes
/// every read pass the in-ral gate; the projection still goes through
/// the OS-sandbox machinery because `saw_fs` is true.
#[cfg(unix)]
fn projecting_caps() -> Capabilities {
    Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![ral_core::path::NormalizedPrefix::from_surface("/")],
            write_prefixes: vec![ral_core::path::NormalizedPrefix::from_surface("/")],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    }
}

/// Render a path without a trailing platform separator.  On hosts where
/// `std::env::temp_dir()` returns `"/tmp/"` (a trailing slash), the bare
/// `display().to_string()` mismatches `Shell::cwd()`'s output (which is
/// canonicalised and never carries a trailing separator).  Trimming the
/// trailing separator here makes the comparison portable without
/// dropping the macOS `/var` ↔ `/private/var` firmlink fallback below.
/// The `len > 1` guard preserves the root "/" itself.
fn display_no_trailing_sep(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if s.len() > 1 {
        s.trim_end_matches(std::path::MAIN_SEPARATOR).to_string()
    } else {
        s
    }
}

// ── (1) Top-level persistence ───────────────────────────────────────────

/// Two sequential top-level calls share state: a `let` from the first
/// call is visible in the second.  This is the load-bearing property of
/// the run's install-mobile-on-Ok rule, and the whole reason exarch's
/// tool-call harness routes through the top-level run door instead of
/// the block boundary.
#[test]
fn top_level_let_persists_across_calls() {
    let mut shell = fresh_shell();
    top_level(&mut shell, "let persist_n = 41").expect("first run");
    let result = top_level(&mut shell, "return $[$persist_n + 1]").expect("second run");
    assert_eq!(result, Value::Int(42));
}

// ── (2) Top-level partial effects ───────────────────────────────────────

/// A top-level run that fails partway through still installs the
/// mobile mutations made before the failure.  This locks in the run's
/// install-on-Error rule: bindings made before a fatal command survive
/// into the next run; bindings *after* the failure do not exist because
/// that line never ran.
#[test]
fn top_level_partial_effects_persist_on_error() {
    let mut shell = fresh_shell();
    // `cat /nonexistent` fails between the two `let` lines.  The whole
    // run returns Err, but the pre-failure binding is in the mobile,
    // which the top-level run installs unconditionally.
    let _ = top_level(
        &mut shell,
        "let pre_fail_x = 1\ncat /nonexistent\nlet post_fail_y = 2",
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

/// A destructuring bind is all-or-nothing even across the run door.  The
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

/// A `try` that recovers clears the failure's status: the run's transport
/// status is 0 and `$?` reads 0 in the next run.  The baseline half proves
/// the failing command really does set the register, so the recovered half
/// is a genuine reset rather than a status that was never written.
#[cfg(unix)]
#[test]
fn recovered_try_clears_the_status_register() {
    let status = |shell: &mut Shell, source: &str| match shell.run(RunRequest {
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
        fork: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.status(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    };

    let mut shell = fresh_shell();
    let failing = "cat /nonexistent 2> /dev/null";
    assert_ne!(
        status(&mut shell, failing),
        0,
        "the baseline failure must report a non-zero status"
    );
    assert_ne!(shell.last_status(), 0, "the baseline failure must set `$?`");

    let recovered = format!("try {{ {failing} }} {{ |_e| return () }}");
    assert_eq!(
        status(&mut shell, &recovered),
        0,
        "a recovered `try` must report success to the transport"
    );
    assert_eq!(
        shell.last_status(),
        0,
        "`$?` must not see past the recovery"
    );
}

// ── (3) Top-level cwd ───────────────────────────────────────────────────

/// `cd` in one top-level run is visible to subsequent runs: a later
/// `cwd` reflects the directory set by the earlier `cd`.  This locks
/// in that `context.cwd` lives in the mobile and rides the top-level
/// install-mobile contract.
#[test]
fn top_level_cd_persists_across_calls() {
    let mut shell = fresh_shell();
    // /tmp exists on every Unix CI image.  On Windows the test would
    // need a different stable directory; gating to unix mirrors how
    // the existing `within_dir_carries_to_external_command` test gates.
    let tmp = std::env::temp_dir();
    let tmp_disp = display_no_trailing_sep(&tmp);
    top_level(&mut shell, &format!("cd '{tmp_disp}'")).expect("cd should succeed");
    let result = top_level(&mut shell, "cwd").expect("cwd should succeed");
    // `Shell::cwd()` returns the canonicalised path; comparing string
    // forms tolerates the macOS `/var` ↔ `/private/var` firmlink the
    // same way `within_dir_carries_to_external_command` does.
    let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or_else(|_| tmp.clone()));
    let got = match result {
        Value::String(s) => s,
        other => panic!("cwd must return a String, got {other:?}"),
    };
    assert!(
        got == tmp_disp || got == canon,
        "cwd after cd: expected {tmp_disp:?} or {canon:?}, got {got:?}"
    );
}

/// The path slot holds a String, but the checker cannot ask for a non-empty
/// one, and `""` would resolve to the cwd — a `cd` that succeeded nowhere.  So
/// the empty path is refused by name, and the cwd stays where it was.
#[test]
fn cd_refuses_the_empty_path_and_stays_put() {
    let mut shell = fresh_shell();
    let before = shell.cwd();
    match top_level(&mut shell, "let d = ''\ncd $d") {
        Err(Break::Error(e)) => assert!(
            e.message.contains("the empty string names no directory"),
            "expected the empty-path refusal, got {:?}",
            e.message
        ),
        other => panic!("the empty path must be refused, got {other:?}"),
    }
    assert_eq!(shell.cwd(), before, "a refused `cd` moves nothing");
}

/// `cd` is a native, so the env answers its bare head before any handler
/// frame: a `cd` handler installs, sits shadowed, and the directory really
/// moves.  `^cd` skips the env and is where that handler answers.
#[test]
fn a_cd_handler_is_shadowed_at_the_bare_head_and_answers_under_caret() {
    let mut shell = fresh_shell();
    let tmp = std::env::temp_dir();
    let tmp_disp = display_no_trailing_sep(&tmp);
    let handler = "[handlers: [cd: { |args| return intercepted }]]";

    let moved = top_level(
        &mut shell,
        &format!("within {handler} {{ cd '{tmp_disp}'; cwd }}"),
    )
    .expect("the bare head is an env hit on the native");
    let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or_else(|_| tmp.clone()));
    let got = match moved {
        Value::String(s) => s,
        other => panic!("cwd must return a String, got {other:?}"),
    };
    assert!(
        got == tmp_disp || got == canon,
        "the shadowed handler must not divert the move: expected {tmp_disp:?} or {canon:?}, got {got:?}"
    );

    assert_eq!(
        top_level(
            &mut shell,
            &format!("within {handler} {{ ^cd '{tmp_disp}' }}")
        )
        .expect("`^cd` reaches the handler frame"),
        Value::String("intercepted".into()),
    );
}

// ── (4) Block non-leakage — grant ───────────────────────────────────────

/// A `let` inside `grant [...] { ... }` must not be visible afterwards:
/// the block boundary discards the body's post-run mobile.  Capabilities
/// inside the grant are an empty exec policy + permissive fs/net — the
/// minimum that still parses and lets `let` run.  Mirrors the policy
/// shape exarch's tool grants use without inducing OS sandbox setup
/// (no fs/net key → no projection → local transport, isolating this
/// to the block-discipline question).
#[test]
fn block_grant_does_not_leak_let_binding() {
    let mut shell = fresh_shell();
    // grant accepts a capabilities map; `[exec: [:]]` is the smallest
    // valid shape that still attenuates without making the body fail.
    let _ = top_level(&mut shell, "grant [exec: [:]] { let leak_grant = 1 }")
        .expect("grant body should succeed");
    assert!(
        shell.scope_lookup("leak_grant").is_none(),
        "`let` inside grant must not leak into the parent env"
    );
}

// ── (5) Block non-leakage — within / try / guard / audit ────────────────

/// `within [...] { let ... }` does not leak: the body is a forced thunk
/// and routes through `eval_block`, which drops the post-run mobile.
#[test]
fn block_within_does_not_leak_let_binding() {
    let mut shell = fresh_shell();
    let tmp = std::env::temp_dir().display().to_string();
    let _ = top_level(
        &mut shell,
        &format!("within [dir: '{tmp}'] {{ let leak_within = 1 }}"),
    )
    .expect("within body should succeed");
    assert!(
        shell.scope_lookup("leak_within").is_none(),
        "`let` inside within must not leak into the parent env"
    );
}

/// `try { body } { handler }` runs its body as a forced thunk; the
/// body's bindings must not leak even on the success path.  Handler
/// returns Unit so the test stays focused on the body discipline.
#[test]
fn block_try_does_not_leak_let_binding() {
    let mut shell = fresh_shell();
    // `let` returns Unit; the handler must agree on the body's return
    // type for `try`'s type rule to accept the pair (body: F V for any V,
    // handler: ErrorRec → F V).  Both arms `let` so both return Unit.
    let _ = top_level(
        &mut shell,
        "try { let leak_try = 1 } { |_e| let _ignore = 0 }",
    )
    .expect("try body should succeed");
    assert!(
        shell.scope_lookup("leak_try").is_none(),
        "`let` inside try must not leak into the parent env"
    );
}

/// `guard { body } { cleanup }` forces both body and cleanup as
/// thunks; neither's bindings may leak.  The `let` is in the body
/// arm — the cleanup arm is exercised in the existing fuzz tests.
#[test]
fn block_guard_does_not_leak_let_binding() {
    let mut shell = fresh_shell();
    // Body `let` returns Unit; cleanup `echo` also returns Unit so the
    // two branches typecheck.  Matches the existing
    // `guard_runs_cleanup_on_success` fuzz-test shape.
    let _ = top_level(&mut shell, "guard { let leak_guard = 1 } { echo done }")
        .expect("guard body should succeed");
    assert!(
        shell.scope_lookup("leak_guard").is_none(),
        "`let` inside guard must not leak into the parent env"
    );
}

/// `audit { body }` records the body's audit trail but the body is
/// still a forced thunk — its mobile is discarded under the same
/// block rule.
#[test]
fn block_audit_does_not_leak_let_binding() {
    let mut shell = fresh_shell();
    let _ =
        top_level(&mut shell, "audit { let leak_audit = 1 }").expect("audit body should succeed");
    assert!(
        shell.scope_lookup("leak_audit").is_none(),
        "`let` inside audit must not leak into the parent env"
    );
}

// ── (6) Block cwd non-leakage ───────────────────────────────────────────

/// A `cd` inside a block-shaped scope must not be visible afterwards.
/// We use `grant` (not `within [dir: ...]`) so the test isn't masked
/// by `within`'s own dir-override mechanism — `grant`'s body is a plain
/// forced thunk and any `cd` inside lands in the body's discarded
/// mobile.  The post-block cwd must equal the pre-block cwd.
#[test]
fn block_grant_does_not_leak_cd() {
    let mut shell = fresh_shell();
    let before = shell.cwd();
    let tmp = std::env::temp_dir().display().to_string();
    // Use root-fs caps so the `cd` target check passes inside the block.
    let _ = top_level(&mut shell, &format!("grant [exec: [:]] {{ cd '{tmp}' }}"))
        .expect("grant body should succeed");
    let after = shell.cwd();
    assert_eq!(
        before, after,
        "block `cd` must not leak into the parent's logical cwd"
    );
}

// ── (7) Sandbox parity ──────────────────────────────────────────────────

/// Persistence, partial effects, and cwd must look the same whether or
/// not a fs/net projection is active.  A `grant` body now always
/// evaluates locally (milestone 5 of
/// `decisions/260617_sandbox-external-children`), so an active projection
/// must not change top-level state semantics: parents observe identical
/// behaviour either way.
#[cfg(unix)]
#[test]
fn sandbox_parity_top_level_persistence() {
    let mut shell = fresh_shell();
    let caps = projecting_caps();
    top_level_under(&mut shell, caps.clone(), "let parity_n = 41").expect("first run");
    let result = top_level_under(&mut shell, caps, "return $[$parity_n + 1]").expect("second run");
    assert_eq!(
        result,
        Value::Int(42),
        "let must persist across calls regardless of an active projection"
    );
}

/// Partial effects under an active fs projection: same install-on-error
/// rule as without one.  A pre-failure binding survives; a post-failure
/// binding does not.
#[cfg(unix)]
#[test]
fn sandbox_parity_top_level_partial_effects() {
    let mut shell = fresh_shell();
    let caps = projecting_caps();
    let _ = top_level_under(
        &mut shell,
        caps,
        "let pre_parity_x = 1\ncat /nonexistent\nlet post_parity_y = 2",
    );
    assert!(
        shell.scope_lookup("pre_parity_x").is_some(),
        "pre-failure `let` must persist under sandbox projection"
    );
    assert!(
        shell.scope_lookup("post_parity_y").is_none(),
        "post-failure `let` must not appear under sandbox projection"
    );
}

// ── (8) Concurrent blocks: spawn / par on the worker-thread path ────────────

/// A bare `spawn { ... }` without any active fs/net projection must run
/// the body locally and produce a value through the normal `await`
/// record.  Locks in that the worker thread evaluates the body via
/// `with_scope(eval_comp(body))` directly — no top-level/block boundary
/// ceremony, no confined transport, just a thread running a block.
#[test]
fn spawn_without_projection_still_runs() {
    let mut shell = fresh_shell();
    let result = top_level(
        &mut shell,
        "let h = !{spawn { return 41 }}\nlet awaited = await $h\nreturn $awaited[value]",
    )
    .expect("spawn/await without projection");
    assert_eq!(result, Value::Int(41));
}

/// `par` is now prelude code over `spawn`/`await`.  With a small
/// concurrency window it must still emit results in input order — that
/// stability is the documented difference between `par` and a raw
/// fan-out, and is what callers like `hash-tree.ral` rely on.
#[test]
fn par_prelude_returns_values_in_order() {
    let mut shell = fresh_shell();
    let result = top_level(&mut shell, "!{par { |x| return $[$x * 2] } [1, 2, 3] 2}")
        .expect("par returns a list");
    assert_eq!(
        result,
        Value::list(vec![Value::Int(2), Value::Int(4), Value::Int(6)]),
        "par must preserve input order under a bounded concurrency window"
    );
}

/// A `spawn` inside a grant body is usable *within that body*: under an
/// active fs projection the grant body now evaluates **locally** (there is
/// no grant-body IPC boundary anymore — milestone 4 of
/// `decisions/260617_sandbox-external-children`), so the handle is an
/// ordinary process-local reference to a worker thread and `await` on it
/// returns the worker's value.
///
/// This replaces the old `handle_cannot_cross_confined_eval`, which
/// asserted a handle could not cross the confined-eval IPC boundary. That
/// boundary is gone: a `grant` body is a dynamic effect scope, not a
/// process boundary, so a handle created and consumed inside it just works.
/// (Confinement now lives at external dispatch, exercised by
/// `sandbox_fail_closed.rs`, not at grant-body entry.)
#[cfg(unix)]
#[test]
fn handle_is_usable_inside_local_grant_body() {
    let mut shell = fresh_shell();
    let caps = projecting_caps();
    let result = top_level_under(
        &mut shell,
        caps,
        "let h = !{spawn { return 1 }}\nlet awaited = await $h\nreturn $awaited[value]",
    );
    match result {
        Ok(v) => assert_eq!(
            v,
            Value::Int(1),
            "a spawn handle created and awaited under an active projection must work locally"
        ),
        Err(e) => panic!("handle should be usable in a local grant body, got error: {e:?}"),
    }
}

/// `poll` on a still-running block returns `` `pending ``.  The block
/// sleeps far longer than the microseconds before the poll, so the
/// observation is deterministic; the handle is cancelled afterward so no
/// worker outlives the test.
#[test]
fn poll_pending_on_a_running_block() {
    let mut shell = fresh_shell();
    let result = top_level(
        &mut shell,
        "let h = !{spawn { sleep 30 }}\n\
         let polled = poll $h\n\
         let pending = case $polled [`settled: { |_| return false }, `pending: { |_| return true }]\n\
         cancel $h\n\
         return $pending",
    )
    .expect("poll a running handle");
    assert_eq!(result, Value::Bool(true));
}

/// `poll` then `await` on the same handle agree: once `poll` settles a
/// handle (draining the channel into the cache), the subsequent `await`
/// returns that same cached record rather than blocking on an already-
/// emptied channel.
#[test]
fn poll_then_await_returns_the_cached_record() {
    let mut shell = fresh_shell();
    let result = top_level(
        &mut shell,
        "let h = !{spawn { return 7 }}\n\
         let _ = await $h\n\
         let sampled = poll $h\n\
         let polled = case $sampled [`settled: { |s| case $s[outcome] [`ok: { |v| return $v }, `err: { |_| return -2 }] }, `pending: { |_| return -1 }]\n\
         let awaited = await $h\n\
         return $[$polled + $awaited[value]]",
    )
    .expect("poll then await");
    assert_eq!(result, Value::Int(14));
}

/// The `is-done` prelude predicate reports `false` while a block runs
/// and `true` once it has settled.  The running observation uses a long
/// sleep (cancelled afterward); the settled observation forces
/// completion with an `await` first.
#[test]
fn is_done_reports_false_then_true() {
    let mut shell = fresh_shell();
    let running = top_level(
        &mut shell,
        "let h = !{spawn { sleep 30 }}\n\
         let finished = !{is-done $h}\n\
         cancel $h\n\
         return $finished",
    )
    .expect("is-done on a running handle");
    assert_eq!(running, Value::Bool(false));

    let settled = top_level(
        &mut shell,
        "let h = !{spawn { return 1 }}\n\
         let _ = await $h\n\
         is-done $h",
    )
    .expect("is-done on a settled handle");
    assert_eq!(settled, Value::Bool(true));
}

/// `poll` errors on a cancelled handle exactly as `await` does: a
/// detached handle has no result to sample, and the error is recoverable
/// via `try`.
#[test]
fn poll_on_a_cancelled_handle_errors() {
    let mut shell = fresh_shell();
    let result = top_level(
        &mut shell,
        "let h = !{spawn { sleep 30 }}\n\
         cancel $h\n\
         try { poll $h; return false } { |_| return true }",
    )
    .expect("try recovers the poll error");
    assert_eq!(result, Value::Bool(true));
}

/// `cd` under an active fs projection persists into the next run, same
/// as without one.  Validates that `context.cwd` rides the mobile through
/// the (now always local) top-level dispatch under a projection.
#[cfg(unix)]
#[test]
fn sandbox_parity_top_level_cd() {
    let mut shell = fresh_shell();
    let caps = projecting_caps();
    let tmp = std::env::temp_dir();
    let tmp_disp = display_no_trailing_sep(&tmp);
    top_level_under(&mut shell, caps.clone(), &format!("cd '{tmp_disp}'"))
        .expect("cd under projection");
    let result = top_level_under(&mut shell, caps, "cwd").expect("cwd under projection");
    let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or_else(|_| tmp.clone()));
    let got = match result {
        Value::String(s) => s,
        other => panic!("cwd returned non-String: {other:?}"),
    };
    assert!(
        got == tmp_disp || got == canon,
        "cwd after cd under projection: expected {tmp_disp:?} or {canon:?}, got {got:?}"
    );
}

// ── (9) Same-thread β-step flow matrix: lambda vs forced block ───────────
//
// A forced block (`!{ … }`) and an applied lambda (`f x`) both run their
// body in place on the caller's shell, sharing run / session / local
// state by identity
// (`decisions/260620_same-thread-body-shares-the-session`).  They differ in
// exactly two observable places — the entry `$?` and the fold-back set —
// which these tests pin.

/// A lambda body enters with a *fresh* `$?`, not the caller's: define a
/// function whose body sets no status, prime the caller's `$?` to a
/// non-zero sentinel, call it, and observe `$?` come back 0 — the lambda
/// reset it on entry and folded the (untouched) 0 back.
#[test]
fn lambda_enters_with_fresh_status() {
    let mut shell = fresh_shell();
    top_level(&mut shell, "let f = { |_| return () }").expect("define f");
    shell.set_last_status(7);
    top_level(&mut shell, "f ()").expect("call f");
    assert_eq!(
        shell.last_status(),
        0,
        "a lambda body enters with a fresh $? (0), not the caller's 7; its \
         body set none, so 0 folds back"
    );
}

/// A forced block, by contrast, inherits the caller's `$?` (it clones the
/// caller's mobile) and — with a body that sets none — folds it back
/// unchanged.  Same body shape as the lambda above, opposite outcome.
#[test]
fn forced_block_keeps_caller_status_when_body_sets_none() {
    let mut shell = fresh_shell();
    shell.set_last_status(7);
    top_level(&mut shell, "!{ return () }").expect("forced block");
    assert_eq!(
        shell.last_status(),
        7,
        "a forced block keeps the caller's $? when its body sets none"
    );
}

/// A lambda folds its body's final status back to the caller, replacing
/// whatever the caller held.  `return $[1 == 2]` returns a false Bool,
/// which the evaluator records as `$? = 1`.
#[test]
fn lambda_folds_back_body_status() {
    let mut shell = fresh_shell();
    top_level(&mut shell, "let gg = { |_| return $[1 == 2] }").expect("define gg");
    shell.set_last_status(5);
    top_level(&mut shell, "gg ()").expect("call gg");
    assert_eq!(
        shell.last_status(),
        1,
        "the lambda body's status (1, from a false comparison) folds back, \
         replacing the caller's 5"
    );
}

/// A forced block likewise folds its body's final status back.
#[test]
fn forced_block_folds_back_body_status() {
    let mut shell = fresh_shell();
    shell.set_last_status(5);
    top_level(&mut shell, "!{ $[1 == 2] }").expect("forced block");
    assert_eq!(
        shell.last_status(),
        1,
        "the block body's status (1, from a false comparison) folds back, \
         replacing the caller's 5"
    );
}

/// A `cd` inside a lambda body PERSISTS after the call: `cwd` is part of
/// the lambda fold-back set.  Checked against the same canonicalisation a
/// plain top-level `cd` produces.
#[test]
fn lambda_cd_persists() {
    let tmp = std::env::temp_dir();
    let tmp_disp = display_no_trailing_sep(&tmp);

    let mut shell = fresh_shell();
    let before = shell.cwd();
    top_level(
        &mut shell,
        &format!("let h = {{ |_| cd '{tmp_disp}'; return () }}\nh ()"),
    )
    .expect("lambda body cd");
    let after = shell.cwd();

    assert_ne!(before, after, "a `cd` inside a lambda body must persist");
    let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or_else(|_| tmp.clone()));
    assert!(
        after == tmp_disp || after == canon,
        "lambda `cd` must land in the temp dir: expected {tmp_disp:?} or {canon:?}, got {after:?}"
    );
}

/// A `cd` inside a forced block is DISCARDED — the block's mobile (cwd and
/// all) dies on exit; only `$?` folds back.  The direct-force analogue of
/// `block_grant_does_not_leak_cd`.
#[test]
fn forced_block_discards_cd() {
    let tmp = std::env::temp_dir();
    let tmp_disp = display_no_trailing_sep(&tmp);

    let mut shell = fresh_shell();
    let before = shell.cwd();
    top_level(&mut shell, &format!("!{{ cd '{tmp_disp}' }}")).expect("forced block cd");
    assert_eq!(
        before,
        shell.cwd(),
        "a `cd` inside a forced block must not persist"
    );
}

/// A command run inside a lambda body is recorded into the *enclosing*
/// `audit { … }` trail: the body shares the active audit trail by identity,
/// exactly as a forced block does.
#[test]
fn function_body_records_into_enclosing_audit() {
    let mut shell = fresh_shell();
    top_level(&mut shell, "let emit = { |_| echo audited-from-fn }").expect("define emit");
    let tree = top_level(&mut shell, "audit { emit () }").expect("audit body");
    let children = match &tree {
        Value::Map(m) => match m.get("children") {
            Some(Value::List(ch)) => ch.iter().cloned().collect::<Vec<_>>(),
            other => panic!("audit tree must have a list `children` field; got {other:?}"),
        },
        other => panic!("audit {{ … }} must return a Map; got {other:?}"),
    };
    let saw_echo = children.iter().any(|c| match c {
        Value::Map(m) => {
            matches!(m.get("kind"), Some(Value::String(k)) if k == "command")
                && matches!(
                    m.get("argv"),
                    Some(Value::List(argv))
                        if matches!(argv.get(0), Some(Value::String(s)) if s == "echo")
                )
        }
        _ => false,
    });
    assert!(
        saw_echo,
        "the `echo` inside the function body must appear as its own observation in the \
         enclosing audit tree; children = {children:?}"
    );
}

/// The lone command-kind observation among an `audit { … }` tree's children,
/// as `(argv, status)`.  Filtered by `kind`, since the trail also records
/// writes and reads alongside commands.
fn only_command_child(tree: &Value) -> (Vec<Value>, Value) {
    let children = match tree {
        Value::Map(m) => match m.get("children") {
            Some(Value::List(ch)) => ch,
            other => panic!("audit tree must have a list `children` field; got {other:?}"),
        },
        other => panic!("audit {{ … }} must return a Map; got {other:?}"),
    };
    let commands: Vec<_> = children
        .iter()
        .filter(|c| {
            matches!(c, Value::Map(m) if matches!(m.get("kind"), Some(Value::String(k)) if k == "command"))
        })
        .collect();
    assert_eq!(
        commands.len(),
        1,
        "expected exactly one command observation; got {children:?}"
    );
    match commands[0] {
        Value::Map(m) => (
            match m.get("argv") {
                Some(Value::List(a)) => a.iter().cloned().collect(),
                other => panic!("command observation must have a List `argv`; got {other:?}"),
            },
            m.get("status")
                .cloned()
                .expect("command observation must have `status`"),
        ),
        other => panic!("audit child must be a Map; got {other:?}"),
    }
}

/// Forcing an arity-0 native (`!$cwd`) records exactly the same command
/// observation — same argv and status — as applying that native as a
/// command head (`cwd`).  The `!`-force arm must run under its own audit
/// frame just like the command-dispatch path, not silently unaudited.
#[test]
fn forcing_an_arity_zero_native_records_like_applying_it() {
    let mut shell = fresh_shell();
    let forced = top_level(&mut shell, "audit { !$cwd }").expect("forced native under audit");
    let applied = top_level(&mut shell, "audit { cwd }").expect("applied native under audit");
    assert_eq!(
        only_command_child(&forced),
        only_command_child(&applied),
        "`!$cwd` must record the same command observation as `cwd`"
    );
}
