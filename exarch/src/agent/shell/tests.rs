//! Call-boundary bookkeeping and panic recovery.

use super::*;
use crate::agent::outcome::Outcome;
use crate::agent::testkit::*;
use crate::provider::scripted::{Reply, Script};
use crate::record::Voice;
use ral_core::process::DurableRoot;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use ral_core::{Shell, Value};
use std::borrow::Cow;

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
    let mut session = Agent::for_test("system").unwrap();
    {
        let log = &mut session.body.log;
        log.append_user("material that must stay bound".into(), None, Voice::Human)
            .unwrap();
        log.append_assistant(
            genai::chat::ChatMessage::assistant("the answer stays in the binding"),
            Vec::new(),
            None,
        )
        .unwrap();
    }
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session
        .ral(
            &DurableRoot::default().child(),
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
        "every tool result closes with its turn: {result}"
    );
}

#[test]
fn worker_panic_preserves_completed_bindings_and_clean_context() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(PANIC_BUILTINS));
    let baseline_grant_depth = probe_count(&session, ral_core::test_access::grant_depth);
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let a4_x = 7")]))
                .then(Reply::tool_calls(vec![ral_call("c2", "a4-panic-now")]))
                .then(Reply::text("recovered")),
        ),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let provider = session.handle().current_provider();
    let outcome = session.deliberate(
        &session.mailbox.life().child(),
        &provider,
        Some(("compute then crash".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)), "{outcome:?}");
    assert_eq!(
        probe_count(&session, ral_core::test_access::grant_depth),
        baseline_grant_depth,
        "the panicking call's grant frame must not leak into the next run"
    );
    assert!(
        scope_has(&mut session, "a4_x"),
        "a completed binding survives a later panic"
    );
    assert!(session.is_ready());
    let provider = scripted("test-model", Script::new().then(Reply::text("ok")));
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("continue".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(
        matches!(outcome, Ok(Outcome::Complete)),
        "healed shell: {outcome:?}"
    );
}

#[test]
fn large_binding_install_warns_on_its_own_run_stderr() {
    let mut session = dressed_trunk_over(
        |_| {},
        |shell| {
            shell.arm_binding_lease(ral_core::shell::runtime::BindingLease {
                idle_calls: 1_000_000,
                large_binding_bytes: 8,
            });
        },
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session
        .ral(
            &DurableRoot::default().child(),
            "let large_binding_x = 'well over eight bytes long'",
            5,
            &emit,
        )
        .text;
    assert_eq!(result.matches("large binding `large_binding_x`").count(), 1);
    assert!(result.contains("held in session memory"));
    let (tx2, _rx2) = crate::bus::channel();
    let emit2 = Emitter::new(tx2, session.id());
    let result2 = session
        .ral(&DurableRoot::default().child(), "return 1", 5, &emit2)
        .text;
    assert!(!result2.contains("large binding"));
}

#[cfg(unix)]
#[test]
fn a_non_zero_exit_carries_the_audit_of_what_already_stands() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session
        .ral(
            &DurableRoot::default().child(),
            "exarch-agents `reply 'the work stands'\n/bin/sh -c 'exit 3'",
            10,
            &emit,
        )
        .text;
    assert!(result.contains("EXIT: 3"), "{result}");
    let (remedy, audit) = (
        result.find("recovery: this non-zero exit raised"),
        result.find("audit: this call had already staged your reply"),
    );
    assert!(audit.is_some(), "a committed act must be audited: {result}");
    assert!(remedy < audit, "the audit comes last: {result}");
}

#[cfg(unix)]
#[test]
fn the_wall_names_the_workers_that_survived_it() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session
        .ral(
            &DurableRoot::default().child(),
            "let sleeper = spawn \"sleeper\" { /bin/sleep 20 }\n/bin/sleep 20",
            2,
            &emit,
        )
        .text;
    assert!(result.contains("EXIT: 124"), "{result}");
    assert!(
        result.contains("(sleeper)"),
        "the surviving worker is named: {result}"
    );
}

#[test]
fn a_call_that_returns_says_nothing_about_its_workers() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session
        .ral(
            &DurableRoot::default().child(),
            "let ok = spawn \"ok\" { return 1 }",
            10,
            &emit,
        )
        .text;
    assert!(
        !result.contains("(ok)"),
        "a completed call holds its handle: {result}"
    );
}

#[cfg(unix)]
#[test]
fn a_non_zero_exit_with_a_live_birth_names_the_orphan() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session
        .ral(
            &DurableRoot::default().child(),
            "let sleeper = spawn \"sleeper\" { /bin/sleep 20 }\n/bin/sh -c 'exit 3'",
            10,
            &emit,
        )
        .text;
    assert!(result.contains("EXIT: 3"), "{result}");
    assert!(
        result.contains("(sleeper)"),
        "a live birth survives a non-zero exit: {result}"
    );
}

#[test]
fn panic_after_prune_does_not_resurrect_binding() {
    let mut session = dressed_trunk_over(
        |runtime| runtime.install_builtins(PANIC_BUILTINS),
        |shell| {
            shell.arm_binding_lease(ral_core::shell::runtime::BindingLease {
                idle_calls: 2,
                large_binding_bytes: u64::MAX,
            });
        },
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    for command in ["let panic_prune_x = 1", "let _spin1 = 0", "let _spin2 = 0"] {
        session.ral(&DurableRoot::default().child(), command, 5, &emit);
    }
    assert!(!scope_has(&mut session, "panic_prune_x"));
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call(
                    "c3",
                    "let survives_y = 9",
                )]))
                .then(Reply::tool_calls(vec![ral_call("c4", "a4-panic-now")]))
                .then(Reply::text("recovered")),
        ),
    );
    let provider = session.handle().current_provider();
    let outcome = session.deliberate(
        &session.mailbox.life().child(),
        &provider,
        Some(("compute then crash".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)), "{outcome:?}");
    assert!(scope_has(&mut session, "survives_y"));
    assert!(!scope_has(&mut session, "panic_prune_x"));
}

#[test]
fn boot_names_survive_past_the_idle_bound() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let boot_name = session
        .engine()
        .read(|t| t.bindings())
        .unwrap()
        .into_iter()
        .next()
        .expect("boot seeds at least one binding")
        .name;
    for _ in 0..(shell_eval::BINDING_IDLE_CALLS + 5) {
        session.ral(
            &DurableRoot::default().child(),
            "let _boot_spin = 0",
            5,
            &emit,
        );
    }
    assert!(
        scope_has(&mut session, &boot_name),
        "baseline bindings never age out"
    );
}
