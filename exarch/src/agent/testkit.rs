//! Fixtures shared by two or more `mod tests` across the crate; a helper only
//! one of them needs belongs in that file instead.

use crate::agent::build::Kin;
use crate::agent::fleet::{Claim, Fleet, TenantId, Unborn};
use crate::agent::handle::Shared;
use crate::agent::{Agent, AgentHandle, ProviderHandle, RootConfig, RootEngine, SPAWN_FUEL, Trunk};
use crate::app::Scratch;
use crate::bus::{AgentOutcome, Child, Emitter, Inbox, Mailbox, ParkMode};
use crate::provider::Toolset;
use crate::provider::scripted::Script;
use crate::provider::{Provider, ToolCall};
use crate::record::{Settlement, Voice};
use ral_core::HostSurface;
use ral_core::Runtime;
use ral_core::Shell;
use ral_core::Value;
use ral_core::engine::EngineInstaller;
use ral_core::io::terminal::TerminalState;
use ral_core::own::{OwnDir, TestDir};
use ral_core::process::DurableRoot;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) fn inbox(agent: &Agent) -> &Inbox {
    &agent.inbox
}

type Dress = Box<dyn FnOnce(&mut Shell)>;
type RuntimeDress = Box<dyn FnOnce(&mut Runtime)>;

thread_local! {
    static DRESS: std::cell::Cell<Option<Dress>> = const { std::cell::Cell::new(None) };
    static RUNTIME_DRESS: std::cell::Cell<Option<RuntimeDress>> = const { std::cell::Cell::new(None) };
}

fn dressed_runtime(spec: &ral_core::protocol::EngineSpec) -> Result<Runtime, String> {
    let mut runtime = crate::boot::engine_runtime(spec)?;
    if let Some(dress) = RUNTIME_DRESS.take() {
        dress(&mut runtime);
    }
    Ok(runtime)
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

/// An [`Agent::for_test`] trunk whose runtime `runtime` fits out at boot.
pub(crate) fn dressed_trunk(runtime: impl FnOnce(&mut Runtime) + 'static) -> Agent {
    dressed_trunk_over(runtime, |_| {})
}

/// [`dressed_trunk`], with `shell` also fitting out its root.
pub(crate) fn dressed_trunk_over(
    runtime: impl FnOnce(&mut Runtime) + 'static,
    shell: impl FnOnce(&mut Shell) + 'static,
) -> Agent {
    Agent::for_test_with(crate::agent::TestTrunk {
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
        &ral_core::protocol::Attach::new(
            "bare",
            ral_core::path::EnginePath::Host(temp.clone()),
            ral_core::path::EnginePath::Host(temp),
        ),
    )
    .expect("a bare engine boots")
}

/// Every worker `session`'s engine holds, handles included, so a test can
/// watch one outlive the engine.
pub(crate) fn workers(session: &Agent) -> Vec<ral_core::shell::runtime::WorkerEntry> {
    let crate::agent::engine::EngineKind::Identity(transport) = session.engine.kind() else {
        panic!("a test trunk runs an identity engine");
    };
    ral_core::test_access::workers(&transport)
}

pub(crate) fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// A synthetic owner without an engine or log.
pub(crate) struct TestAgentSpec {
    pub(crate) name: String,
    pub(crate) kin: Kin,
    pub(crate) idle: Duration,
    pub(crate) fuel: u32,
    pub(crate) search: bool,
}

impl TestAgentSpec {
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            kin: Kin::Trunk(Trunk::Attended),
            idle: Duration::ZERO,
            fuel: 0,
            search: false,
        }
    }

    pub(crate) fn child(name: &str, parent: &AgentHandle, work: Mailbox) -> Self {
        Self {
            kin: Kin::Child {
                parent: parent.clone(),
                work,
            },
            ..Self::new(name)
        }
    }
}

/// Retaining its handle never retains this owner or its census entry.
pub(crate) struct TestAgent {
    handle: AgentHandle,
    pub(crate) inbox: Inbox,
    claim: Claim,
}

impl TestAgent {
    pub(crate) fn tenant(&self) -> TenantId {
        self.claim.id()
    }
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.handle.end();
    }
}

impl std::ops::Deref for TestAgent {
    type Target = AgentHandle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

/// A synthetic owner using production adoption, names and census entries.
pub(crate) fn test_agent(fleet: &Arc<Fleet>, spec: TestAgentSpec) -> Result<TestAgent, Unborn> {
    let TestAgentSpec {
        name,
        kin,
        idle,
        fuel,
        search,
    } = spec;
    let claim = fleet.claim(&name)?;
    let id = crate::agent::fresh_id();
    let started = Instant::now()
        .checked_sub(idle)
        .expect("test birth fits the clock");
    let role = kin.role();
    let lodging = kin.lodging(|| {
        Child::new(
            name.as_str().into(),
            started,
            crate::record::Emitter::none(),
        )
    });
    let (under, _life, inbox) = fleet.lodge(lodging)?;
    let handle = AgentHandle(Arc::new(Shared {
        id,
        name,
        log_dir: fleet.launch.run_dir().clone(),
        run_dir: fleet.launch.run_dir().clone(),
        started,
        role,
        fuel,
        search,
        provider: ProviderHandle::new(scripted("test-model", Script::new())),
        under,
        mailbox: inbox.life_mailbox(),
    }));
    claim.bind(&handle);
    Ok(TestAgent {
        handle,
        inbox,
        claim,
    })
}

/// A boundary read through one of `test_access`'s count probes — unlike
/// `scope_has` it ticks no epoch and no ledger.
pub(crate) fn probe_count(
    session: &Agent,
    probe: impl FnOnce(&dyn ral_core::carrier::Transport) -> Result<u64, ral_core::carrier::ProbeError>,
) -> u64 {
    session
        .engine
        .read(probe)
        .expect("an identity engine never severs")
}

/// Whether `name` resolves, asked through a real eval — which ticks the ral
/// epoch and the binding-lease ledger, so an idle-arithmetic test must count it.
pub(crate) fn scope_has(session: &Agent, name: &str) -> bool {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    // Discarded, so a block- or handle-valued binding still settles on data.
    let text = session
        .ral(
            &DurableRoot::default().child(),
            &format!("let _ = ${name}"),
            5,
            &emit,
        )
        .text;
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
pub(crate) fn close_exchange(session: &Agent, prompt: &str, answer: &str) {
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
pub(crate) fn tmp(tag: &str) -> TestDir {
    OwnDir::for_test(&format!("a4-{tag}")).expect("test scratch dir")
}

/// Swap `session`'s live provider — what `/model` mutates in production — so
/// a forked child can run its own independent script rather than share its
/// spawner's, the way a real `` exarch-agents `start `` does.
pub(crate) fn set_provider(session: &Agent, provider: Arc<Provider>) {
    session.handle.0.provider.swap(provider);
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

/// An attended or embedded trunk through the real root path.
pub(crate) fn trunk(attended: bool) -> Agent {
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
pub(crate) fn chat_trunk() -> Agent {
    root(Trunk::Attended, Toolset::default())
}

/// A trunk whose network policy denies hosted search — the ceiling a fork inherits,
/// in the direction the ordinary fixture does not cover.
pub(crate) fn searchless_trunk() -> Agent {
    Agent::for_test_with(crate::agent::TestTrunk {
        search: false,
        ..crate::agent::TestTrunk::new("system")
    })
    .expect("searchless test trunk")
}

fn root(trunk: Trunk, tools: Toolset) -> Agent {
    // The run dir sits beside the scratch, which the engine owns, so the
    // trunk's whole footprint goes when the trunk does.
    let scratch = Scratch::for_test(crate::app::EXARCH, "trunk").expect("scratch dir");
    let run_dir = scratch.test_sibling("run").expect("run dir");
    Agent::root(
        RootConfig {
            system: "system".into(),
            run_dir,
            trunk,
            tools,
            allow_schedule: false,
            resume_on_reset: false,
            disk_warn_bytes: None,
            fuel: SPAWN_FUEL,
            search: true,
            factory: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
            roles: crate::provider::roles::Roles::default(),
        },
        RootEngine::Identity {
            scratch: Arc::new(scratch),
            cwd: ral_core::host::cwd().expect("test process has a cwd"),
            grants: ral_core::authority::policy::GrantStack::default(),
            terminal: ral_core::io::terminal::TerminalState::default(),
        },
        scripted("test-model", Script::new()),
    )
    .expect("root trunk")
}

/// A child of `parent` made the production way: `` exarch-agents `start `` forks
/// the parent's shell and parks it, and the host adopts it, so the child
/// carries the parent's whole scope and builtins, unlike [`Agent::fork_named`].
pub(crate) fn started_child(parent: &Agent, name: &str, grant: &str) -> Agent {
    use crate::agent::engine::{Engine, EngineKind};
    use crate::enquiry::{Agents, ForkClaim, Request, Start};
    use ral_core::carrier::Host;
    use ral_core::data::FOValue;
    use ral_core::data::datum::Datum as _;
    use ral_core::protocol::{EnquiryError, Run};
    use ral_core::source::SourceOrigin;

    struct Adopt {
        parent: AgentHandle,
        mailbox: Mailbox,
        log: crate::agent::LogCell,
        fleet: Arc<Fleet>,
        transport: Arc<ral_core::carrier::IdentityTransport>,
        name: String,
        child: std::sync::Mutex<Option<Agent>>,
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
            let claim = self.fleet.claim(&self.name).expect("a free name");
            let tenancy = claim.occupy(
                self.transport
                    .adopt_parked(id)
                    .map(Engine::adopted)
                    .expect("the fork this run parked"),
            );
            let child = Agent::child_of(
                (&self.parent, self.mailbox.clone()),
                &self.log.borrow(),
                &self.fleet,
                tenancy,
                self.parent.current_provider(),
                spec.memory,
            )
            .expect("an adopted child is born");
            *self.child.lock().unwrap() = Some(child);
            Ok(FOValue::Unit)
        }
    }

    let EngineKind::Identity(transport) = parent.engine.kind() else {
        panic!("a test parent runs an identity engine")
    };
    let host = Arc::new(Adopt {
        parent: parent.handle(),
        mailbox: parent.inbox.mailbox(),
        log: parent.log.clone(),
        fleet: parent.fleet.clone(),
        transport: transport.clone(),
        name: name.to_string(),
        child: std::sync::Mutex::default(),
    });
    let script = format!(
        "exarch-agents `start [prompt: #'go'#, name: '{name}', type: `amnemon, \
         grant: {grant}, model: `inherit]"
    );
    let report = ral_core::carrier::dispatch_to_report(
        &*transport,
        Run::captured(script.as_str(), SourceOrigin::named("<test>")),
        host.clone(),
        None,
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
/// park: end its life once the reply stands. A peer that never replies
/// quiesces on its own and the watcher simply retires with it.
pub(crate) fn drive_peer(child: &mut Agent, provider: Arc<Provider>) -> AgentOutcome {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    set_provider(child, provider);
    let attending = Arc::new(AtomicBool::new(true));
    let watcher = {
        let agent = child.handle();
        let attending = attending.clone();
        std::thread::spawn(move || {
            while attending.load(Ordering::Acquire) {
                if agent.has_reply() {
                    agent.end();
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

// ── worker registry: `/clear` cascade ─────────────────────────────────

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

static WORKER_REGISTRY_TEST_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed("test-clear-block-forever"),
    scheme_test_clear_block_forever,
    "test-only: block until cancelled.",
    BuiltinBody::Static(builtin_test_clear_block_forever),
)];
pub(crate) static WORKER_REGISTRY_TEST_BUILTINS: &[BuiltinEntry] =
    &WORKER_REGISTRY_TEST_BUILTINS_ARR;

/// Poll `inbox` for the next exchange-boundary item — a child's notice lands
/// here; any opening is declined.
pub(crate) fn wait_for_settle(inbox: &crate::bus::Inbox) -> crate::bus::Next {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(drawn) = inbox.draw(|_| ParkMode::Quiesce) {
            return drawn.next;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child did not settle within the timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Draw the next item and open its exchange, as `attend` does.
pub(crate) fn open_next(inbox: &crate::bus::Inbox) -> crate::bus::Next {
    let crate::bus::Drawn { next, opening } = inbox
        .draw(|_| ParkMode::Quiesce)
        .expect("an item is queued");
    opening.expect("the item opens an exchange").open();
    next
}

/// Draw the next item, declining any opening.
pub(crate) fn pop(inbox: &Inbox) -> Option<crate::bus::Next> {
    inbox.draw(|_| ParkMode::Quiesce).map(|drawn| drawn.next)
}

/// Draw the next item, which must be a child's notice, and return its settlement.
pub(crate) fn pop_settled(inbox: &Inbox) -> Settlement {
    match pop(inbox) {
        Some(crate::bus::Next::Item(crate::bus::Item::Agent { settled, .. })) => settled,
        other => panic!("expected a child's notice, got {other:?}"),
    }
}
