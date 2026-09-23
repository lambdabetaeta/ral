//! Fixtures shared by two or more `mod tests` across the crate; a helper only
//! one of them needs belongs in that file instead.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]

use crate::agent::cancel::InterruptTarget;
use crate::agent::{
    Agent, Avatar, NoControl, ProviderHandle, RecordedAccount, RootConfig, RootSeat, SPAWN_FUEL,
    cancel,
};
use crate::bootstrap::Scratch;
use crate::bus::{AgentOutcome, Emitter, Mailbox};
use crate::fleet::Fleet;
use crate::provider::scripted::Script;
use crate::provider::{Provider, ToolCall};
use ral_core::Shell;
use ral_core::Value;
use ral_core::engine::EngineInstaller;
use ral_core::serial::FOValue;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use ral_core::typecheck::{Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Dress = Box<dyn FnOnce(&mut Shell)>;

thread_local! {
    static DRESS: std::cell::Cell<Option<Dress>> = const { std::cell::Cell::new(None) };
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
)]
fn dressed_boot(attach: &ral_core::protocol::Attach) -> Result<ral_core::engine::Booted, String> {
    let mut booted = crate::bootstrap::engine_boot_shell(attach)?;
    if let Some(dress) = DRESS.take() {
        dress(&mut booted.shell);
    }
    Ok(booted)
}

static DRESSED: [EngineInstaller; 1] = [EngineInstaller {
    tag: crate::shell_eval::builtins::INSTALLER_TAG,
    boot: dressed_boot,
    narrow: crate::policy::base_layer,
}];

/// Exarch's recipe, then `dress`, for the next boot on this thread: a recipe
/// is a `fn`, and `boot` runs it on the calling thread. Spent on that boot,
/// so a `/clear` reboots undressed.
pub(crate) fn dressed(dress: impl FnOnce(&mut Shell) + 'static) -> &'static [EngineInstaller] {
    DRESS.set(Some(Box::new(dress)));
    &DRESSED
}

/// A [`Avatar::for_test`] trunk whose engine `dress` fits out at boot.
pub(crate) fn dressed_trunk(dress: impl FnOnce(&mut Shell) + 'static) -> Avatar {
    Avatar::for_test_with(crate::agent::TestTrunk {
        installers: dressed(dress),
        ..crate::agent::TestTrunk::new("system")
    })
    .expect("dressed test trunk")
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
)]
fn bare_boot(_attach: &ral_core::protocol::Attach) -> Result<ral_core::engine::Booted, String> {
    Ok(ral_core::engine::Booted {
        shell: Shell::new(ral_core::io::TerminalState::default()),
        keep: Box::new(()),
    })
}

static BARE: [EngineInstaller; 1] = [EngineInstaller {
    tag: "bare",
    boot: bare_boot,
    narrow: |_, _| Err("a bare test engine hatches no children".into()),
}];

/// An engine over a bare shell: no prelude, no surface, nothing to reach
/// but its control and its probes.
pub(crate) fn bare_transport() -> ral_core::protocol::IdentityTransport {
    let temp = std::env::temp_dir();
    ral_core::protocol::IdentityTransport::boot(
        &BARE,
        &ral_core::protocol::Attach::new("bare", temp.clone(), temp),
    )
    .expect("a bare engine boots")
}

/// Every worker `session`'s engine holds, handles included, so a test can
/// watch one outlive the engine.
pub(crate) fn workers(session: &Avatar) -> Vec<ral_core::types::WorkerEntry> {
    let crate::agent::seat::SeatKind::Identity(transport) = session.seat.kind() else {
        panic!("a test trunk sits on an identity seat");
    };
    ral_core::test_access::workers(&transport)
}

pub(crate) fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// What a fleet-focused test varies about a synthetic agent — no seat, no
/// shell, nothing an attend loop would touch.  Every other field is the inert
/// placeholder [`test_agent`] fills in.
pub(crate) struct TestAgentSpec {
    pub(crate) name: String,
    pub(crate) cancel: cancel::Token,
    pub(crate) reach: InterruptTarget,
    pub(crate) mailbox: Mailbox,
    /// `None` builds a root, `Some` a reporting child of that agent.
    pub(crate) parent: Option<Arc<Agent>>,
    /// How long this agent has already been idle at birth: `started` is
    /// backdated by it, so a test can stand past an idle threshold without
    /// waiting the threshold out.
    pub(crate) idle: Duration,
    pub(crate) caps: ral_core::types::GrantStack,
    pub(crate) fuel: u32,
    pub(crate) returns: bool,
    pub(crate) search: bool,
    pub(crate) allow_schedule: bool,
    pub(crate) disk_warn_bytes: Option<u64>,
    pub(crate) egress: crate::egress::Egress,
    pub(crate) dial: Option<Arc<dyn crate::agent::Dial>>,
    pub(crate) bureau: Arc<crate::provider::Bureau>,
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
            reach: InterruptTarget::new(
                ral_core::protocol::Transport::control(&bare_transport()).clone(),
            ),
            mailbox: crate::bus::Inbox::new().mailbox(),
            parent: None,
            idle: Duration::ZERO,
            caps: ral_core::types::GrantStack::root(),
            fuel: 0,
            returns: false,
            search: false,
            allow_schedule: false,
            disk_warn_bytes: None,
            egress: crate::egress::Egress::for_test(),
            dial: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
            system_base: String::new(),
            index: crate::prompt::BuiltinIndex::resolve(
                Shell::new(ral_core::io::TerminalState::default())
                    .builtin_names()
                    .map(str::to_string)
                    .collect(),
            ),
        }
    }
}

/// The `Arc<Agent>` a real fork or root carries, born exactly as one is —
/// enrolled, adopted, leased — so the caller holds the only strong reference
/// and dropping it settles the agent.
///
/// The log directory a test agent names: one no `create_dir_all` can make,
/// so a test that unexpectedly writes a log fails loudly rather than
/// quietly leaving a directory behind on the machine that ran it.
///
/// Unix has `/nonexistent`, under a root the test user cannot write.
/// Windows has no such root: a leading `/` is drive-relative, so the same
/// spelling resolves to `C:\nonexistent`, which an ordinary user
/// creates without trouble — and did, littering the filesystem root.  Its
/// twin names a path beneath the null device, under which no directory
/// can exist at all.
#[cfg(unix)]
const UNWRITABLE_LOG_DIR: &str = "/nonexistent/test-agent";
#[cfg(windows)]
const UNWRITABLE_LOG_DIR: &str = r"\\.\NUL\test-agent";

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
        idle,
        caps,
        fuel,
        returns,
        search,
        allow_schedule,
        disk_warn_bytes,
        egress,
        dial,
        bureau,
        system_base,
        index,
    } = spec;
    let id = crate::agent::fresh_id();
    let consumer = parent.as_ref().map(|p| p.mailbox().stamp());
    let agent = Arc::new(Agent {
        id,
        name,
        log_dir: PathBuf::from(UNWRITABLE_LOG_DIR),
        started: std::time::Instant::now()
            .checked_sub(idle)
            .expect("a test's backdated birth stays inside the monotonic clock"),
        system: Arc::from(""),
        system_base: system_base.into(),
        index,
        caps,
        parent,
        children: Mutex::new(Vec::new()),
        fuel,
        provider: ProviderHandle::new(scripted("test-model", Script::new())),
        interactive: false,
        tools: crate::shell_eval::tools::Toolset::offered(false),
        search,
        returns,
        allow_schedule,
        disk_warn_bytes,
        egress,
        dial,
        bureau,
        cancel,
        reach,
        mailbox,
        schedules: crate::fleet::schedule::ScheduleRegistry::new(),
        pins: Arc::default(),
        status: Mutex::new(crate::agent::Status {
            rest: None,
            reply: None,
            awaiting: std::collections::BTreeSet::new(),
        }),
        consumer,
    });
    fleet.enrol(&agent)?;
    Ok(agent)
}

/// A capturing run of `src`, under the lattice top.
pub(crate) fn source_run(src: &str) -> ral_core::protocol::Run {
    ral_core::protocol::Run {
        program: ral_core::protocol::Program::Source(src.into()),
        script_name: "<test>".into(),
        caps: ral_core::types::GrantStack::root(),
        wall: None,
        deferred_lease: None,
        worker_cap: None,
        io: ral_core::RunIo::Capture,
        terminal: ral_core::RequestedTerminalAccess::Denied,
        stdin: ral_core::RunStdin::Empty,
        trail: None,
    }
}

/// A boundary read through one of `test_access`'s count doors — unlike
/// `scope_has` it ticks no epoch and no ledger.
pub(crate) fn probe_count(
    session: &Avatar,
    door: impl FnOnce(&dyn ral_core::protocol::Transport) -> Result<u64, ral_core::protocol::ProbeError>,
) -> u64 {
    session
        .seat
        .read(door)
        .expect("an identity seat never severs")
}

/// Whether `name` resolves, asked through a real eval — which ticks the ral
/// epoch and the binding-lease ledger, so an idle-arithmetic test must count it.
pub(crate) fn scope_has(session: &mut Avatar, name: &str) -> bool {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    // Discarded, so a block- or handle-valued binding still settles on data.
    let (content, _) = session.ral(&format!("let _ = ${name}"), 5, &emit);
    if content.lines().any(|line| line == "EXIT: 0") {
        return true;
    }
    assert!(
        content.contains(&format!("undefined variable: ${name}")),
        "scope probe for `{name}` neither succeeded nor reported an undefined variable: {content}"
    );
    false
}

/// Close one exchange on `session`'s own log — the closed span every context
/// test needs before it has anything addressable to name. The log is private
/// to this module, so a test outside it reaches an exchange through here.
pub(crate) fn close_exchange(session: &Avatar, prompt: &str, answer: &str) {
    let mut log = session.log.lock();
    log.append_user(prompt.to_string(), None)
        .expect("a test prompt");
    log.append_assistant(
        genai::chat::ChatMessage::assistant(answer),
        Vec::new(),
        None,
    )
    .expect("a test answer");
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
/// `` exarch-agents `start `` does.
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

/// A trunk whose network policy denies hosted search — the ceiling a fork inherits,
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
            caps: ral_core::types::GrantStack::root(),
            run_dir,
            resume: None,
            run_lock: None,
            model: "test-model".into(),
            account: RecordedAccount::for_test("test"),
            allow_schedule: false,
            interactive,
            chat,
            thinking_tool: false,
            disk_warn_bytes: None,
            fuel: SPAWN_FUEL,
            egress: crate::egress::Egress::for_test(),
            dial: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
        },
        RootSeat::Identity {
            scratch: Arc::new(scratch),
            cwd: std::env::current_dir().expect("test process has a cwd"),
            terminal: ral_core::io::TerminalState::default(),
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
