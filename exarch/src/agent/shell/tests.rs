//! `Avatar::ral`'s call-boundary bookkeeping — binding-lease pruning, the
//! large-binding warning, worker retention, the audit and the surviving
//! workers a raise owes the model — and the panic recovery those boundaries
//! rest on.

use super::*;
use crate::agent::deliberate;
use crate::agent::testkit::*;
use crate::provider::scripted::{Reply, Script};
use ral_core::Shell;
use ral_core::Value;
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use std::borrow::Cow;

/// Stands in for any Rust panic the evaluator can raise mid-eval.
fn builtin_panic_now(_args: &[Value], _mooring: &Mooring, _shell: &mut Shell) -> Settled<Value> {
    panic!("a4 test: deliberate mid-eval panic");
}

fn scheme_panic_now(_u: &mut Unifier) -> Scheme {
    mk_scheme(&[], &[], thunk(pure(Ty::Unit)))
}

static PANIC_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed("a4-panic-now"),
    scheme_panic_now,
    "test-only: panic the evaluator mid-eval.",
    BuiltinBody::Static(builtin_panic_now),
)];
static PANIC_BUILTINS: &[BuiltinEntry] = &PANIC_BUILTINS_ARR;

#[test]
fn let_bound_context_read_does_not_echo_the_transcript() {
    let session = Avatar::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        log.append_user("material that must stay bound".into(), None)
            .unwrap();
        log.append_assistant(
            genai::chat::ChatMessage::assistant("the answer stays in the binding"),
            Vec::new(),
            None,
        )
        .unwrap();
    }
    // Held, not read: the emitter's far end must outlive the call.
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            "let ctx = exarch-transcript `read [turns: [1, 2]]",
            5,
            &emit,
        )
        .text;
    assert!(
        !result.contains("material that must stay bound")
            && !result.contains("the answer stays in the binding"),
        "a let-bound read must not echo its value: {result}"
    );
    assert!(
        result.ends_with("\nTURN: 2"),
        "every tool result closes with the id of the turn it closes: {result}"
    );
}

/// A panic mid-eval must preserve what completed calls bound and leave the
/// dynamic context clean.  Driven through the real `attend` loop, so the
/// recovery under test is the engine's own run door catching the unwind.
#[test]
fn worker_panic_preserves_completed_bindings_and_clean_context() {
    let mut session = dressed_trunk(|shell| shell.install_builtins(PANIC_BUILTINS));
    let baseline_grant_depth = probe_count(&session, ral_core::test_access::grant_depth);

    // The panicking second call surfaces to the model as an ordinary
    // failed tool result, which is why a third, closing reply follows.
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let a4_x = 7")]))
            .then(Reply::tool_calls(vec![ral_call("c2", "a4-panic-now")]))
            .then(Reply::text("recovered")),
    );
    session.agent.provider.swap(provider);
    session.seed("compute then crash".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let _ = session.attend(&emit);

    // No grant frame leaked out of the panicking call's
    // `with_capabilities`.  Read before the scope probe below, which is
    // itself a call.
    assert_eq!(
        probe_count(&session, ral_core::test_access::grant_depth),
        baseline_grant_depth,
        "the panicking call's grant frame must not leak into the next run"
    );
    // The completed call's binding survives the panic.
    assert!(
        scope_has(&session, "a4_x"),
        "a binding from a completed tool call must survive a later call's panic"
    );
    // The attend loop handed the session back ready for a fresh prompt.
    assert!(
        session.is_ready(),
        "attend must leave the session ReadyForUser even after a worker panic"
    );

    let provider2 = scripted("test-model", Script::new().then(Reply::text("ok")));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    match session.deliberate(&provider2, Some("continue".into()), None, &emit) {
        Ok(deliberate::Outcome::Complete) => {}
        other => panic!("next exchange on the healed shell must complete, got {other:?}"),
    }
}

/// An oversize session-scope install warns on the installing run's own
/// stderr — model-facing in the tool result, not a frontend card — and a
/// later run that installs nothing new stays quiet.  The idle bound is
/// armed out of reach, so only the size axis is in play.
#[test]
fn large_binding_install_warns_on_its_own_run_stderr() {
    let session = dressed_trunk(|shell| {
        shell.arm_binding_lease(ral_core::types::BindingLease {
            idle_calls: 1_000_000,
            large_binding_bytes: 8,
        });
    });

    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
    let result = session
        .ral(
            "let large_binding_x = 'well over eight bytes long'",
            5,
            &emit,
        )
        .text;

    let warnings = result.matches("large binding `large_binding_x`").count();
    assert_eq!(warnings, 1, "exactly one warning per offending install");
    assert!(
        result.contains("held in session memory"),
        "the warning carries the file-path recommendation"
    );

    // `return` binds nothing, so no install meets the threshold again.
    let (tx2, _rx2) = crate::bus::channel();
    let emit2 = Emitter::with_mailbox(tx2, session.agent.id, session.inbox.mailbox());
    let result2 = session.ral("return 1", 5, &emit2).text;
    assert!(
        !result2.contains("large binding"),
        "nothing newly installed must warn again"
    );
}

/// The audit belongs to every raise, not only to the wall: a call that
/// staged its reply and then died on a command's non-zero exit still made
/// that reply stand, so the audit rides that stderr too — last, after the
/// non-zero-exit branch's own remedy.
#[cfg(unix)]
#[test]
fn a_non_zero_exit_carries_the_audit_of_what_already_stands() {
    let session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

    let result = session
        .ral(
            "exarch-agents `reply 'the work stands'\n/bin/sh -c 'exit 3'",
            10,
            &emit,
        )
        .text;

    assert!(
        result.contains("EXIT: 3"),
        "the command's own exit is the tool exit; content was: {result}"
    );
    let (remedy, audit) = (
        result.find("recovery: this non-zero exit raised"),
        result.find("audit: this call had already staged your reply"),
    );
    assert!(
        audit.is_some(),
        "a committed act must be audited on a non-zero exit too; content was: {result}"
    );
    assert!(
        remedy < audit,
        "the audit comes last, after the branch's own remedy; content was: {result}"
    );
}

/// A worker `defer`red before the wall outlives it — moored to the session
/// root, out of the foreground cancel's reach — while the handle binding
/// that named it is gone with the unwind.  The timeout stderr must say so,
/// naming the work by joining this dispatch's own trail births against the
/// live `` `workers `` probe, or the model is left unable to `await` and
/// unaware there is anything to await.
#[cfg(unix)]
#[test]
fn the_wall_names_the_workers_that_survived_it() {
    let session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

    let result = session
        .ral("let deferred = defer { sleep 20 }\nsleep 20", 2, &emit)
        .text;

    assert!(
        result.contains("EXIT: 124"),
        "the wall exits 124; content was: {result}"
    );
    assert!(
        result.contains("`block at tool call, line 1`"),
        "the surviving worker is named by the line that deferred it; content was: {result}"
    );
}

/// The same probe, on a call the wall did not cut: a completed call's
/// worker is nobody's orphan, so nothing is said about it.
#[test]
fn a_call_that_returns_says_nothing_about_its_workers() {
    let session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

    let result = session.ral("let ok = defer { return 1 }", 10, &emit).text;

    assert!(
        !result.contains("`block at tool call, line 1`"),
        "a call that returned holds its own handle; content was: {result}"
    );
}

/// The widening this wave deliberately introduces: a live birth outlives a
/// routine non-zero exit exactly as it outlives the wall, so it draws the
/// same sentence there too.
#[cfg(unix)]
#[test]
fn a_non_zero_exit_with_a_live_birth_names_the_orphan() {
    let session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

    let result = session
        .ral(
            "let deferred = defer { sleep 20 }\n/bin/sh -c 'exit 3'",
            10,
            &emit,
        )
        .text;

    assert!(
        result.contains("EXIT: 3"),
        "the command's own exit is the tool exit; content was: {result}"
    );
    assert!(
        result.contains("`block at tool call, line 1`"),
        "a non-zero exit leaves a live birth standing exactly as the wall does; content was: {result}"
    );
}

/// A panic cannot resurrect a name pruned before it: the run door's
/// checkpoint is taken at the panicking call's own entry, by which time
/// the prune is already part of the state being checkpointed.
#[test]
fn panic_after_prune_does_not_resurrect_binding() {
    let mut session = dressed_trunk(|shell| {
        shell.install_builtins(PANIC_BUILTINS);
        shell.arm_binding_lease(ral_core::types::BindingLease {
            idle_calls: 2,
            large_binding_bytes: u64::MAX,
        });
    });

    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.ral("let panic_prune_x = 1", 5, &emit);
    session.ral("let _spin1 = 0", 5, &emit);
    session.ral("let _spin2 = 0", 5, &emit);

    // The probe is itself a call, so it may prune the idle `_spin` names
    // too — nothing below asserts on those.
    assert!(
        !scope_has(&session, "panic_prune_x"),
        "panic_prune_x must already be pruned before the panicking call"
    );

    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call(
                "c3",
                "let survives_y = 9",
            )]))
            .then(Reply::tool_calls(vec![ral_call("c4", "a4-panic-now")]))
            .then(Reply::text("recovered")),
    );
    session.agent.provider.swap(provider);
    session.seed("compute then crash".into());
    let _ = session.attend(&emit);

    // `survives_y` first: each probe ticks the armed idle bound of 2, and
    // reading a name renews it, so the second probe cannot prune it.
    assert!(
        scope_has(&session, "survives_y"),
        "a completed call's binding must survive a later call's panic"
    );
    assert!(
        !scope_has(&session, "panic_prune_x"),
        "the pruned name must not resurrect across the panic's rollback"
    );
}

/// Against the real `BINDING_IDLE_CALLS`, not a re-armed test bound: a
/// boot-seeded name is baseline and never ages out.
#[test]
fn boot_names_survive_past_the_idle_bound() {
    let session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);

    let boot_name = session
        .seat
        .read(|t| t.bindings())
        .expect("an identity seat never severs")
        .into_iter()
        .next()
        .expect("the boot sequence seeds at least one binding")
        .name;

    for _ in 0..(shell_eval::BINDING_IDLE_CALLS + 5) {
        session.ral("let _boot_spin = 0", 5, &emit);
    }
    assert!(
        scope_has(&session, &boot_name),
        "a boot-seeded (baseline) name must survive past the idle bound"
    );
}
