//! Fixtures shared by two or more `mod tests` across the crate; a helper only
//! one of them needs belongs in that file instead.

#![allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]

use crate::agent::{Agent, Avatar, NoControl, ProviderHandle, RecordedAccount, RootConfig, RootSeat, SPAWN_FUEL, cancel};
use crate::bootstrap::Scratch;
use crate::agent::cancel::EvalReach;
use crate::bus::{AgentOutcome, Emitter, Mailbox};
use crate::fleet::Fleet;
use crate::provider::scripted::Script;
use crate::provider::{Provider, ToolCall};
use ral_core::Shell;
use ral_core::Value;
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use ral_core::typecheck::{Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(crate) fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// What a fleet-focused test varies about a synthetic agent — no seat, no
/// shell, nothing an attend loop would touch.  Every other field is the inert
/// placeholder [`test_agent`] fills in.
pub(crate) struct TestAgentSpec {
    pub(crate) name: String,
    pub(crate) cancel: cancel::Token,
    pub(crate) reach: EvalReach,
    pub(crate) mailbox: Mailbox,
    /// `None` builds a root, `Some` a reporting child of that agent.
    pub(crate) parent: Option<Arc<Agent>>,
    pub(crate) caps: ral_core::types::Capabilities,
    pub(crate) fuel: u32,
    pub(crate) returns: bool,
    pub(crate) search: bool,
    pub(crate) allow_schedule: bool,
    pub(crate) disk_warn_bytes: Option<u64>,
    pub(crate) egress: crate::egress::Egress,
    pub(crate) dial: Option<Arc<dyn crate::agent::Dial>>,
    /// Still carrying [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`], like
    /// [`Agent::system_base`](crate::agent::Agent) itself.
    pub(crate) system_base: String,
    pub(crate) index: Arc<crate::prompt::BuiltinIndex>,
}

impl TestAgentSpec {
    /// A root under `name`, with handles no assertion inspects.
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            cancel: cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: crate::agent::cancel::InterruptTarget::default(),
            },
            mailbox: crate::bus::Inbox::new().mailbox(),
            parent: None,
            caps: ral_core::types::Capabilities::default(),
            fuel: 0,
            returns: false,
            search: false,
            allow_schedule: false,
            disk_warn_bytes: None,
            egress: crate::egress::Egress::for_test(),
            dial: None,
            system_base: String::new(),
            index: crate::prompt::BuiltinIndex::resolve(&Shell::new(
                ral_core::io::TerminalState::default(),
            )),
        }
    }
}

/// The `Arc<Agent>` a real fork or root carries, born exactly as one is —
/// enrolled, adopted, leased — so the caller holds the only strong reference
/// and dropping it settles the agent.
///
/// # Errors
/// Whatever [`Fleet::enrol`] refuses.
pub(crate) fn test_agent(
    fleet: &Arc<Fleet>,
    spec: TestAgentSpec,
) -> Result<Arc<Agent>, crate::fleet::Unborn> {
    let TestAgentSpec {
        name,
        cancel,
        reach,
        mailbox,
        parent,
        caps,
        fuel,
        returns,
        search,
        allow_schedule,
        disk_warn_bytes,
        egress,
        dial,
        system_base,
        index,
    } = spec;
    let id = crate::agent::fresh_id();
    let consumer = parent.as_ref().map_or(0, |p| p.generation());
    let agent = Arc::new(Agent {
        id,
        name,
        log_dir: PathBuf::from("/nonexistent/test-agent"),
        started: std::time::Instant::now(),
        system: String::new(),
        system_base,
        index,
        caps,
        parent,
        children: Mutex::new(Vec::new()),
        fuel,
        provider: ProviderHandle::new(scripted("test-model", Script::new())),
        interactive: false,
        tool_enabled: false,
        search,
        returns,
        allow_schedule,
        disk_warn_bytes,
        egress,
        dial,
        cancel,
        reach,
        mailbox,
        status: Mutex::new(crate::agent::Status {
            generation: 0,
            rest: None,
            reply: None,
        }),
        consumer,
    });
    fleet.enrol(&agent)?;
    Ok(agent)
}

/// A boundary read — unlike `scope_has` it ticks no epoch and no ledger.
pub(crate) fn probe_int(session: &Avatar, class: &str) -> i64 {
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
pub(crate) fn scope_has(session: &mut Avatar, name: &str) -> bool {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
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
/// `` agents `start `` does.
pub(crate) fn set_provider(session: &Avatar, provider: Arc<Provider>) {
    session.agent.provider.swap(provider);
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

/// A trunk through the real `Avatar::root` path; `interactive` makes it converse.
pub(crate) fn trunk(interactive: bool) -> Avatar {
    root(interactive, false)
}

/// The toolless `--chat` trunk, conversing by construction: the flag is
/// interactive-only.
pub(crate) fn chat_trunk() -> Avatar {
    root(true, true)
}

/// A trunk whose IT policy denies hosted search — the ceiling a fork inherits,
/// in the direction the ordinary fixture does not cover.
pub(crate) fn searchless_trunk() -> Avatar {
    Avatar::for_test_with(crate::agent::TestTrunk {
        egress: crate::egress::Egress::for_test_without_search(),
        ..crate::agent::TestTrunk::new("system")
    })
    .expect("searchless test trunk")
}

fn root(interactive: bool, chat: bool) -> Avatar {
    // The run dir sits beside the scratch, which the seat below owns, so the
    // trunk's whole footprint goes when the trunk does.
    let scratch = Scratch::for_test(crate::bootstrap::EXARCH, "trunk").expect("scratch dir");
    let run_dir = scratch.test_sibling("run").expect("run dir");
    Avatar::root(
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
            dial: None,
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

/// Drive a forked peer to quiescence, returning what its `attend` settles.
///
/// A peer that replies parks for a follow-up rather than ending, exactly as a
/// spawned child does, so this stands in for the parent that would end that
/// park: a terminate once the reply stands.  A peer that never replies
/// quiesces on its own and the watcher simply retires with it.
pub(crate) fn drive_peer(
    child: &mut Avatar,
    provider: Arc<Provider>,
) -> (AgentOutcome, Option<FOValue>) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.agent.id);
    child.agent.provider.swap(provider);
    let attending = Arc::new(AtomicBool::new(true));
    let watcher = {
        let agent = child.agent.clone();
        let attending = attending.clone();
        std::thread::spawn(move || {
            while attending.load(Ordering::Acquire) {
                if agent.has_reply() {
                    agent.cancel(ral_core::process::CancelCause::Explicit);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    };
    let settled = child.attend(&mut NoControl, &emit);
    attending.store(false, Ordering::Release);
    watcher.join().expect("the watcher thread must not panic");
    settled
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
        scheme_test_clear_block_forever,
        "test-only: block until cancelled.",
        BuiltinBody::Static(builtin_test_clear_block_forever),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("test-clear-block-until-released"),
        scheme_test_clear_block_forever,
        "test-only: ignore cancellation until released, then settle on it.",
        BuiltinBody::Static(builtin_test_clear_block_until_released),
    ),
];
pub(crate) static WORKER_REGISTRY_TEST_BUILTINS: &[BuiltinEntry] =
    &WORKER_REGISTRY_TEST_BUILTINS_ARR;
