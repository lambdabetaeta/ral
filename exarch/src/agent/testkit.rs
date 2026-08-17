//! Fixtures shared by two or more `mod tests` across the crate; a helper only
//! one of them needs belongs in that file instead.

#![allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]

use crate::agent::{
    Agent, NoControl, ProviderHandle, RecordedAccount, RootConfig, RootSeat, SPAWN_FUEL,
    render_reply,
};
use crate::bootstrap::Scratch;
use crate::bus::{AgentOutcome, Emitter};
use crate::provider::scripted::Script;
use crate::provider::{Provider, ToolCall};
use ral_core::Shell;
use ral_core::Value;
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{BuiltinTypeRule, mk_scheme, pure, thunk};
use ral_core::typecheck::{Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use std::borrow::Cow;
use std::sync::Arc;

pub(crate) fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// A boundary read — unlike `scope_has` it ticks no epoch and no ledger.
pub(crate) fn probe_int(session: &Agent, class: &str) -> i64 {
    match session.seat.transport().probe(FOValue::Variant {
        label: class.into(),
        payload: None,
    }) {
        Ok(FOValue::Int { value }) => value,
        other => panic!("`{class} probe must answer an Int, got {other:?}"),
    }
}

/// Whether `name` resolves, asked through a real eval — which ticks the ral
/// epoch and the binding-lease ledger, so an idle-arithmetic test must count it.
pub(crate) fn scope_has(session: &mut Agent, name: &str) -> bool {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id);
    let content = session
        .run_shell(format!("probe-{name}"), &format!("${name}"), 5, &emit)
        .content;
    // A closure-valued binding never crosses the seam, but it did resolve.
    if content.contains("VALUE:") || content.contains("not transportable across the host seam") {
        return true;
    }
    assert!(
        content.contains(&format!("undefined variable: ${name}")),
        "scope probe for `{name}` answered neither a VALUE nor an undefined-variable error: {content}"
    );
    false
}

/// A directory for one test, deleted when the returned guard falls.  Hold the
/// guard for as long as the test needs the directory: binding only its path
/// deletes it on the spot.
pub(crate) fn tmp(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("exarch-a4-{tag}-"))
        .tempdir()
        .expect("test scratch dir")
}

/// Swap `session`'s live provider handle — the test-only door onto what
/// `/model` mutates in production, so a forked child can run its own
/// independent script rather than share its spawner's, the way a real
/// `agent-start` does.
pub(crate) fn set_provider(session: &mut Agent, provider: Arc<Provider>) {
    session.provider = ProviderHandle::new(provider);
}

pub(crate) fn ral_call(id: &str, cmd: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: "ral".into(),
        fn_arguments: serde_json::json!({
            "cmd": cmd,
            "description": "a4 test command",
        }),
        thought_signatures: None,
    }
}

/// A trunk through the real `Agent::root` path; `interactive` makes it converse.
pub(crate) fn trunk(interactive: bool) -> Agent {
    root(interactive, false)
}

/// The toolless `--chat` trunk, conversing by construction: the flag is
/// interactive-only.
pub(crate) fn chat_trunk() -> Agent {
    root(true, true)
}

fn root(interactive: bool, chat: bool) -> Agent {
    // The run dir sits beside the scratch, which the seat below owns, so the
    // trunk's whole footprint goes when the trunk does.
    let scratch = Scratch::for_test(crate::bootstrap::EXARCH, "trunk").expect("scratch dir");
    let run_dir = scratch.test_sibling("run").expect("run dir");
    Agent::root(
        RootConfig {
            system: "system".into(),
            caps: ral_core::types::Capabilities::default(),
            run_dir,
            resume: None,
            no_logs: false,
            run_lock: None,
            model: "test-model".into(),
            account: RecordedAccount::for_test("test"),
            allow_schedule: false,
            interactive,
            chat,
            disk_warn_bytes: None,
            fuel: SPAWN_FUEL,
            egress: crate::egress::Egress::for_test(),
            hatchery: None,
        },
        RootSeat::Identity {
            scratch: Arc::new(scratch),
            cwd: std::env::current_dir().expect("test process has a cwd"),
            detach: false,
        },
        scripted("test-model", Script::new()),
    )
    .expect("root trunk")
}

/// Drive a forked peer to quiescence, returning the `(outcome, text)` the spawn
/// site in `shell_eval/tools/agent.rs` would deliver to its parent.
pub(crate) fn drive_peer(child: &mut Agent, provider: Arc<Provider>) -> (AgentOutcome, String) {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id);
    child.provider = ProviderHandle::new(provider);
    let (outcome, payload) = child.attend(&mut NoControl, &emit);
    let text = payload.as_ref().map(render_reply).unwrap_or_default();
    (outcome, text)
}

// ── worker registry: `/clear` cascade, lease-reap drain ───────────────

/// Named apart from `shell_eval/builtins.rs`'s `test-block-forever`, since one
/// test binary installs both.
pub(crate) fn builtin_test_clear_block_forever(
    _args: &[Value],
    mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    loop {
        ral_core::process::check(mooring)?;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

pub(crate) fn scheme_test_clear_block_forever(_u: &mut Unifier) -> Scheme {
    mk_scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
}

/// Gates `test-clear-block-until-released`: while false it cannot settle inside
/// the window where `/clear` has cancelled workers but not yet dropped the inbox.
pub(crate) static CLEAR_RELEASE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn builtin_test_clear_block_until_released(
    _args: &[Value],
    mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    loop {
        if CLEAR_RELEASE.load(std::sync::atomic::Ordering::Acquire) {
            ral_core::process::check(mooring)?;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

static WORKER_REGISTRY_TEST_BUILTINS_ARR: [BuiltinEntry; 2] = [
    BuiltinEntry::new(
        Cow::Borrowed("test-clear-block-forever"),
        BuiltinTypeRule::Scheme(scheme_test_clear_block_forever),
        "test-only: block until cancelled.",
        BuiltinBody::Static(builtin_test_clear_block_forever),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("test-clear-block-until-released"),
        BuiltinTypeRule::Scheme(scheme_test_clear_block_forever),
        "test-only: ignore cancellation until released, then settle on it.",
        BuiltinBody::Static(builtin_test_clear_block_until_released),
    ),
];
pub(crate) static WORKER_REGISTRY_TEST_BUILTINS: &[BuiltinEntry] =
    &WORKER_REGISTRY_TEST_BUILTINS_ARR;
