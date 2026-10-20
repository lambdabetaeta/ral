//! Fixtures shared by two or more `mod tests` across the crate; a helper only
//! one of them needs belongs in that file instead.

use crate::agent::fleet::Fleet;
use crate::agent::{
    Agent, Avatar, Birth, ProviderHandle, Role, RootConfig, RootSeat, SPAWN_FUEL, Trunk,
};
use crate::app::Scratch;
use crate::bus::{AgentOutcome, Emitter, Mailbox};
use crate::cancel::InterruptTarget;
use crate::provider::Toolset;
use crate::provider::scripted::Script;
use crate::provider::{Provider, ToolCall};
use crate::record::Voice;
use ral_core::HostSurface;
use ral_core::Runtime;
use ral_core::Shell;
use ral_core::Value;
use ral_core::engine::EngineInstaller;
use ral_core::terminal::TerminalState;
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

type Dress = Box<dyn FnOnce(&mut Shell)>;
type RuntimeDress = Box<dyn FnOnce(&mut Runtime)>;

thread_local! {
    static DRESS: std::cell::Cell<Option<Dress>> = const { std::cell::Cell::new(None) };
    static RUNTIME_DRESS: std::cell::Cell<Option<RuntimeDress>> = const { std::cell::Cell::new(None) };
}

fn dressed_runtime(spec: &ral_core::protocol::EngineSpec) -> Runtime {
    let mut runtime = crate::boot::engine_runtime(spec);
    if let Some(dress) = RUNTIME_DRESS.take() {
        dress(&mut runtime);
    }
    runtime
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "must match EngineInstaller::root's signature, which can genuinely refuse"
)]
fn dressed_root(root: &ral_core::protocol::Root, shell: &mut Shell) -> Result<(), String> {
    crate::boot::engine_root(root, shell)?;
    if let Some(dress) = DRESS.take() {
        dress(shell);
    }
    Ok(())
}

static DRESSED: [EngineInstaller; 1] = [EngineInstaller {
    tag: crate::shell_eval::builtins::INSTALLER_TAG,
    runtime: dressed_runtime,
    root: dressed_root,
    narrow: crate::policy::base_layer,
}];

/// Exarch's recipe over a runtime `runtime` fits out, for the next boot on
/// this thread: a recipe is a `fn`, and `boot` runs it on the calling thread.
/// Spent on that boot, so a `/clear` reboots undressed.
pub(crate) fn dressed(runtime: impl FnOnce(&mut Runtime) + 'static) -> &'static [EngineInstaller] {
    dressed_over(runtime, |_| {})
}

/// [`dressed`], with `shell` also fitting out the root after its recipe.
pub(crate) fn dressed_over(
    runtime: impl FnOnce(&mut Runtime) + 'static,
    shell: impl FnOnce(&mut Shell) + 'static,
) -> &'static [EngineInstaller] {
    RUNTIME_DRESS.set(Some(Box::new(runtime)));
    DRESS.set(Some(Box::new(shell)));
    &DRESSED
}

/// A [`Avatar::for_test`] trunk whose runtime `runtime` fits out at boot.
pub(crate) fn dressed_trunk(runtime: impl FnOnce(&mut Runtime) + 'static) -> Avatar {
    dressed_trunk_over(runtime, |_| {})
}

/// [`dressed_trunk`], with `shell` also fitting out its root.
pub(crate) fn dressed_trunk_over(
    runtime: impl FnOnce(&mut Runtime) + 'static,
    shell: impl FnOnce(&mut Shell) + 'static,
) -> Avatar {
    Avatar::for_test_with(crate::agent::TestTrunk {
        installers: dressed_over(runtime, shell),
        ..crate::agent::TestTrunk::new("system")
    })
    .expect("dressed test trunk")
}

static BARE: [EngineInstaller; 1] = [EngineInstaller {
    tag: "bare",
    runtime: |_| HostSurface::default().runtime(TerminalState::default()),
    root: |_, _| Ok(()),
    narrow: |_, _| Err("a bare test engine makes no children".into()),
}];

/// An engine over a bare shell: no prelude, no surface, nothing to reach
/// but its control and its probes.
pub(crate) fn bare_transport() -> ral_core::carrier::IdentityTransport {
    let temp = std::env::temp_dir();
    ral_core::carrier::IdentityTransport::boot(
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
/// placeholder [`test_agent`] fills in; what the run shares is the fleet's
/// own [`Launch`](crate::agent::fleet::Launch).
pub(crate) struct TestAgentSpec {
    pub(crate) name: String,
    pub(crate) reach: InterruptTarget,
    pub(crate) mailbox: Mailbox,
    pub(crate) role: Role,
    /// How long this agent has already been idle at birth: `started` is
    /// backdated by it, so a test can stand past an idle threshold without
    /// waiting the threshold out.
    pub(crate) idle: Duration,
    pub(crate) caps: ral_core::capability::GrantStack,
    pub(crate) fuel: u32,
    pub(crate) search: bool,
}

impl TestAgentSpec {
    /// A root under `name`, with handles no assertion inspects.
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            reach: InterruptTarget::new(
                ral_core::carrier::Transport::control(&bare_transport()).clone(),
            ),
            mailbox: crate::bus::Inbox::new().mailbox(),
            role: Role::Converses,
            idle: Duration::ZERO,
            caps: ral_core::capability::GrantStack::root(),
            fuel: 0,
            search: false,
        }
    }
}

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

/// The `Arc<Agent>` a real fork or root carries, born exactly as one is —
/// enrolled and adopted — so the caller holds the only strong reference
/// and dropping it settles the agent.
///
/// # Errors
/// Whatever [`Fleet::enrol`] refuses.
pub(crate) fn test_agent(
    fleet: &Arc<Fleet>,
    spec: TestAgentSpec,
) -> Result<Arc<Agent>, crate::agent::fleet::Unborn> {
    let TestAgentSpec {
        name,
        reach,
        mailbox,
        role,
        idle,
        caps,
        fuel,
        search,
    } = spec;
    let claim = fleet.claim(&name)?;
    let agent = Agent::new(Birth {
        id: crate::agent::fresh_id(),
        name,
        log_dir: PathBuf::from(UNWRITABLE_LOG_DIR),
        started: Instant::now()
            .checked_sub(idle)
            .expect("a test's backdated birth stays inside the monotonic clock"),
        caps,
        role,
        fuel,
        provider: ProviderHandle::new(scripted("test-model", Script::new())),
        search,
        reach,
        mailbox,
    });
    fleet.enrol(&agent, claim)?;
    Ok(agent)
}

/// A boundary read through one of `test_access`'s count probes — unlike
/// `scope_has` it ticks no epoch and no ledger.
pub(crate) fn probe_count(
    session: &Avatar,
    probe: impl FnOnce(&dyn ral_core::carrier::Transport) -> Result<u64, ral_core::carrier::ProbeError>,
) -> u64 {
    session
        .seat
        .read(probe)
        .expect("an identity seat never severs")
}

/// Whether `name` resolves, asked through a real eval — which ticks the ral
/// epoch and the binding-lease ledger, so an idle-arithmetic test must count it.
pub(crate) fn scope_has(session: &Avatar, name: &str) -> bool {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    // Discarded, so a block- or handle-valued binding still settles on data.
    let text = session.ral(&format!("let _ = ${name}"), 5, &emit).text;
    if text.lines().any(|line| line == "EXIT: 0") {
        return true;
    }
    assert!(
        text.contains(&format!("undefined variable: ${name}")),
        "scope probe for `{name}` neither succeeded nor reported an undefined variable: {text}"
    );
    false
}

/// Close one exchange on `session`'s own log — the closed span every context
/// test needs before it has anything addressable to name. The log is private
/// to this module, so a test outside it reaches an exchange through here.
pub(crate) fn close_exchange(session: &Avatar, prompt: &str, answer: &str) {
    let mut log = session.log.borrow_mut();
    log.append_user(prompt.to_string(), None, Voice::Human)
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

/// Swap `session`'s live provider — what `/model` mutates in production — so
/// a forked child can run its own independent script rather than share its
/// spawner's, the way a real `` exarch-agents `start `` does.
pub(crate) fn set_provider(session: &Avatar, provider: Arc<Provider>) {
    session.agent.provider.swap(provider);
}

pub(crate) fn ral_call(id: &str, cmd: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: "ral".into(),
        fn_arguments: serde_json::json!({
            "command": cmd,
            "description": "a4 test command",
        }),
        thought_signatures: None,
    }
}

/// A trunk through the real `Avatar::root` path: attended when `attended`,
/// else the embedded conversing trunk synod drives.
pub(crate) fn trunk(attended: bool) -> Avatar {
    root(
        if attended {
            Trunk::Attended
        } else {
            Trunk::Embedded
        },
        Toolset::offered(false),
    )
}

/// The toolless `--chat` trunk, conversing by construction: the flag is
/// interactive-only.
pub(crate) fn chat_trunk() -> Avatar {
    root(Trunk::Attended, Toolset::default())
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

fn root(trunk: Trunk, tools: Toolset) -> Avatar {
    // The run dir sits beside the scratch, which the seat below owns, so the
    // trunk's whole footprint goes when the trunk does.
    let scratch = Scratch::for_test(crate::app::EXARCH, "trunk").expect("scratch dir");
    let run_dir = scratch.test_sibling("run").expect("run dir");
    Avatar::root(
        RootConfig {
            system: "system".into(),
            caps: ral_core::capability::GrantStack::root(),
            run_dir,
            trunk,
            tools,
            allow_schedule: false,
            resume_on_reset: false,
            disk_warn_bytes: None,
            fuel: SPAWN_FUEL,
            egress: crate::egress::Egress::for_test(),
            factory: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
            roles: crate::provider::roles::Roles::default(),
        },
        RootSeat::Identity {
            scratch: Arc::new(scratch),
            cwd: std::env::current_dir().expect("test process has a cwd"),
            terminal: ral_core::terminal::TerminalState::default(),
        },
        scripted("test-model", Script::new()),
    )
    .expect("root trunk")
}

/// A child of `parent` made the production way: `` exarch-agents `start `` forks
/// the parent's shell and parks it, and the host adopts it, so the child
/// carries the parent's whole scope and builtins — what a genesis-seated
/// [`Avatar::fork_named`] does not.
pub(crate) fn started_child(parent: &Avatar, name: &str) -> Avatar {
    use crate::agent::seat::{Seat, SeatKind};
    use crate::enquiry::{Agents, ForkClaim, Request, Start};
    use ral_core::carrier::Host;
    use ral_core::first_order::FOValue;
    use ral_core::first_order::datum::Datum as _;
    use ral_core::protocol::{EnquiryError, Run};

    struct Adopt {
        parent: Arc<Agent>,
        log: crate::agent::LogCell,
        fleet: Arc<Fleet>,
        transport: Arc<ral_core::carrier::IdentityTransport>,
        name: String,
        child: std::sync::Mutex<Option<Avatar>>,
    }

    impl Host for Adopt {
        fn surface(&self, _val: &FOValue) {}

        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
            let Ok(Request::Agents(Agents::Start(Start {
                spec,
                fork: ForkClaim::Parked(id),
            }))) = Request::decode(&req)
            else {
                panic!("an identity run parks its bequest")
            };
            let seat = self
                .transport
                .adopt_parked(id)
                .map(Seat::adopted)
                .expect("the fork this run parked");
            let child = Avatar::child_of(
                &self.parent,
                &self.log.borrow(),
                &self.fleet,
                self.fleet.claim(&self.name).expect("a free name"),
                seat,
                self.parent.current_provider(),
                spec.memory,
            )
            .expect("an adopted child is born");
            *self.child.lock().unwrap() = Some(child);
            Ok(FOValue::Unit)
        }
    }

    let SeatKind::Identity(transport) = parent.seat.kind() else {
        panic!("a test parent sits on an identity seat")
    };
    let host = Arc::new(Adopt {
        parent: parent.agent.clone(),
        log: parent.log.clone(),
        fleet: parent.fleet.clone(),
        transport: transport.clone(),
        name: name.to_string(),
        child: std::sync::Mutex::default(),
    });
    let script = format!(
        "exarch-agents `start [prompt: #'go'#, name: '{name}', type: `amnemon, grant: `inherit, model: `inherit]"
    );
    let report = ral_core::carrier::dispatch_to_report(
        &*transport,
        Run::captured(script.as_str(), "<test>"),
        host.clone(),
    )
    .expect("an identity engine never severs");
    host.child
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("the run never started a child: {report:?}"))
}

/// Drive a forked peer to quiescence, returning what its `attend` settles.
///
/// A peer that replies parks for a follow-up rather than ending, exactly as a
/// spawned child does, so this stands in for the parent that would end that
/// park: a terminate once the reply stands.  A peer that never replies
/// quiesces on its own and the watcher simply retires with it.
pub(crate) fn drive_peer(child: &mut Avatar, provider: Arc<Provider>) -> AgentOutcome {
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
                    agent.cancel(ral_core::process::CancelCause::Cancelled);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    };
    let settled = child.attend(&emit);
    attending.store(false, Ordering::Release);
    watcher.join().expect("the watcher thread must not panic");
    settled
}

// ── worker registry: `/clear` cascade, lease-reap drain ───────────────

/// Named apart from `shell_eval/builtins.rs`'s `test-block-forever`, since one
/// test binary installs both.
pub(crate) fn builtin_test_clear_block_forever(
    _args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    loop {
        shell.poll()?;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

pub(crate) fn scheme_test_clear_block_forever(_u: &mut Unifier) -> Scheme {
    mk_scheme(&[], &[], thunk(pure(Ty::Unit)))
}

/// Gates `test-clear-block-until-released`: while false it cannot settle inside
/// the window where `/clear` has cancelled workers but not yet dropped the inbox.
pub(crate) static CLEAR_RELEASE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn builtin_test_clear_block_until_released(
    _args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    loop {
        if CLEAR_RELEASE.load(std::sync::atomic::Ordering::Acquire) {
            shell.poll()?;
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

/// Poll `inbox` for the next exchange-boundary item — a spawned child's
/// settled [`crate::bus::AgentResult`] lands here.
pub(crate) fn wait_for_settle(inbox: &crate::bus::Inbox) -> crate::bus::Next {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(item) = inbox.next_item() {
            return item;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child did not settle within the timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
