#![allow(clippy::disallowed_methods)]

//! Semantics of the top-level vs block contract: what persists at the
//! top level, what is discarded across block-shaped scopes (`grant`,
//! `within`, `try`, `guard`, `audit`), and what stays consistent between
//! local and sandbox-confined transport.
//!
//! These tests exercise the **documented** semantics of
//! [`ral_core::evaluator::eval_top_level`] and
//! [`ral_core::evaluator::eval_block`].  They drive the public API
//! the same way a REPL turn (`ral`), a tool call (`exarch`), or a forced
//! thunk inside a `grant { ... }` body would — never reach into
//! internal types.
//!
//! The two-call harness mirrors what exarch's `shell_eval::run_shell`
//! does between consecutive tool calls and what the ral REPL's
//! `execute_input` does between consecutive prompt turns: hold a single
//! [`Shell`] across calls, recompute the typechecker's `bindings` from
//! the live env between calls, and route the body through
//! `eval_top_level`.

mod common;

use ral_core::evaluator;
use ral_core::types::Shell;
#[cfg(unix)]
use ral_core::types::{Capabilities, FsPolicy};
use ral_core::{Comp, CompileOutcome, Value, compile_and_typecheck};

// ── Harness ─────────────────────────────────────────────────────────────

/// Build a `Shell` that mirrors what every front end (`ral`, `exarch`,
/// scripts) ends up with after its bootstrap: prelude registered,
/// default env vars seeded, capabilities at root.  Equivalent to
/// `exarch::bootstrap::boot_shell()` without the TUI / signal-handler
/// pieces, which are irrelevant to boundary semantics.
fn fresh_shell() -> Shell {
    ral_core::host::boot_shell(Default::default(), common::prelude())
}

/// Parse + elaborate + typecheck `source` against the live `shell`'s
/// bindings, exactly as the REPL and exarch's tool harness do between
/// turns.  Panics on parse / type failure — every test below picks
/// source it expects to compile.
fn compile_against(shell: &Shell, source: &str) -> std::sync::Arc<Comp> {
    match compile_and_typecheck(source, shell.session_schemes()) {
        CompileOutcome::Compiled(c) => std::sync::Arc::new(c),
        CompileOutcome::Parse(e) => panic!("parse: {source:?}: {e}"),
        CompileOutcome::Types(errs) => {
            let msgs: Vec<_> = errs.iter().map(|e| e.kind.render_message()).collect();
            panic!("type: {source:?}: {}", msgs.join("; "));
        }
    }
}

/// Run one top-level turn against `shell`, matching exarch's per-tool
/// flow: compile against the live env, then route through the
/// `eval_top_level` boundary.  Returns whatever the body returned (or
/// the body's error).
fn top_level(shell: &mut Shell, source: &str) -> ral_core::types::Settled<Value> {
    let comp = compile_against(shell, source);
    evaluator::eval_top_level(&comp, shell)
}

/// Run one top-level turn under an attenuated `Capabilities` frame,
/// mirroring how exarch wraps its call in `with_capabilities(...)`
/// before handing off to `eval_top_level`.  Used by the sandbox-parity
/// tests below.
#[cfg(unix)]
fn top_level_under(
    shell: &mut Shell,
    caps: Capabilities,
    source: &str,
) -> ral_core::types::Settled<Value> {
    let comp = compile_against(shell, source);
    shell.with_capabilities(caps, |s| evaluator::eval_top_level(&comp, s))
}

/// Capability frame that actually triggers `sandbox_projection()` to
/// return `Some(_)`: any `fs` policy is enough.  Read prefix `/` makes
/// every read pass the in-ral gate; the projection still goes through
/// the OS-sandbox machinery because `saw_fs` is true.
#[cfg(unix)]
fn projecting_caps() -> Capabilities {
    Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/".into()],
            write_prefixes: vec!["/".into()],
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
/// `eval_top_level`'s install-mobile-on-Ok rule, and the whole reason
/// exarch's tool-call harness routes through this boundary instead of
/// the block boundary.
#[test]
fn top_level_let_persists_across_calls() {
    let mut shell = fresh_shell();
    top_level(&mut shell, "let persist_n = 41").expect("first turn");
    let result = top_level(&mut shell, "return $[$persist_n + 1]").expect("second turn");
    assert_eq!(result, Value::Int(42));
}

// ── (2) Top-level partial effects ───────────────────────────────────────

/// A top-level turn that fails partway through still installs the
/// mobile mutations made before the failure.  This locks in the
/// install-on-Error rule documented on `eval_top_level`: bindings made
/// before a fatal command survive into the next turn; bindings *after*
/// the failure do not exist because that line never ran.
#[test]
fn top_level_partial_effects_persist_on_error() {
    let mut shell = fresh_shell();
    // `cat /nonexistent` fails between the two `let` lines.  The whole
    // turn returns Err, but the pre-failure binding is in the mobile,
    // which `eval_top_level` installs unconditionally.
    let _ = top_level(
        &mut shell,
        "let pre_fail_x = 1\ncat /nonexistent\nlet post_fail_y = 2",
    );
    assert!(
        shell.mobile.scope.get("pre_fail_x").is_some(),
        "pre-failure `let` must survive into the next turn"
    );
    assert!(
        shell.mobile.scope.get("post_fail_y").is_none(),
        "post-failure `let` never ran, must not be present"
    );
}

// ── (3) Top-level cwd ───────────────────────────────────────────────────

/// `cd` in one top-level turn is visible to subsequent turns: a later
/// `cwd` reflects the directory set by the earlier `cd`.  This locks
/// in that `logical_cwd` lives in the mobile and rides the top-level
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
    let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or(tmp.clone()));
    let got = match result {
        Value::String(s) => s,
        other => panic!("cwd must return a String, got {other:?}"),
    };
    assert!(
        got == tmp_disp || got == canon,
        "cwd after cd: expected {tmp_disp:?} or {canon:?}, got {got:?}"
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
        shell.mobile.scope.get("leak_grant").is_none(),
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
        shell.mobile.scope.get("leak_within").is_none(),
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
        shell.mobile.scope.get("leak_try").is_none(),
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
        shell.mobile.scope.get("leak_guard").is_none(),
        "`let` inside guard must not leak into the parent env"
    );
}

/// `audit { body }` records the body's execution tree but the body is
/// still a forced thunk — its mobile is discarded under the same
/// block rule.
#[test]
fn block_audit_does_not_leak_let_binding() {
    let mut shell = fresh_shell();
    let _ =
        top_level(&mut shell, "audit { let leak_audit = 1 }").expect("audit body should succeed");
    assert!(
        shell.mobile.scope.get("leak_audit").is_none(),
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
    top_level_under(&mut shell, caps.clone(), "let parity_n = 41").expect("first turn");
    let result = top_level_under(&mut shell, caps, "return $[$parity_n + 1]").expect("second turn");
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
        shell.mobile.scope.get("pre_parity_x").is_some(),
        "pre-failure `let` must persist under sandbox projection"
    );
    assert!(
        shell.mobile.scope.get("post_parity_y").is_none(),
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
        "let h = !{spawn { return 41 }}\nlet r = await $h\nreturn $r[value]",
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
        "let h = !{spawn { return 1 }}\nlet r = await $h\nreturn $r[value]",
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

/// `poll` is the non-blocking dual of `await`.  A handle whose block
/// has already settled (forced here by an `await` first) polls to
/// `` `settled `` whose `` `ok `` outcome carries the block's value,
/// drawn from the shared cache.
#[test]
fn poll_settled_after_completion_carries_the_value() {
    let mut shell = fresh_shell();
    let result = top_level(
        &mut shell,
        "let h = !{spawn { return 42 }}\n\
         let _ = await $h\n\
         let p = poll $h\n\
         case $p [`settled: { |s| case $s[outcome] [`ok: { |v| return $v }, `err: { |_| return -2 }] }, `pending: { |_| return -1 }]",
    )
    .expect("poll a settled handle");
    assert_eq!(result, Value::Int(42));
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
         let p = poll $h\n\
         let pending = case $p [`settled: { |_| return false }, `pending: { |_| return true }]\n\
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
         let p = case $sampled [`settled: { |s| case $s[outcome] [`ok: { |v| return $v }, `err: { |_| return -2 }] }, `pending: { |_| return -1 }]\n\
         let r = await $h\n\
         return $[$p + $r[value]]",
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
         let r = !{is-done $h}\n\
         cancel $h\n\
         return $r",
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

/// `cd` under an active fs projection persists into the next turn, same
/// as without one.  Validates that `logical_cwd` rides the mobile through
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
    let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or(tmp.clone()));
    let got = match result {
        Value::String(s) => s,
        other => panic!("cwd returned non-String: {other:?}"),
    };
    assert!(
        got == tmp_disp || got == canon,
        "cwd after cd under projection: expected {tmp_disp:?} or {canon:?}, got {got:?}"
    );
}
