//! Fixtures shared by two or more `mod tests` across the crate; a helper only
//! one of them needs belongs in that file instead.

use crate::agent::fleet::{Fleet, Name, Unborn};
use crate::agent::handle::Shared;
use crate::agent::{Admission, Agent, AgentHandle, Role, TestTrunk, Trunk};
use crate::bus::{AgentMailbox, Emitter};
use crate::host::Host;
use crate::provider::Toolset;
use crate::provider::scripted::Script;
use crate::provider::{Provider, ToolCall};
use crate::record::{Born, Voice};
use ral_core::Runtime;
use ral_core::Shell;
use ral_core::Value;
use ral_core::engine::EngineInstaller;
use ral_core::own::{OwnDir, TestDir};
use ral_core::process::ScopeNode;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Every worker `session`'s engine holds, handles included, so a test can
/// watch one outlive the engine.
pub(crate) fn workers(session: &Agent) -> Vec<ral_core::shell::runtime::WorkerEntry> {
    let crate::agent::engine::EngineKind::Identity(transport) = session.engine().kind() else {
        panic!("a test trunk runs an identity engine");
    };
    ral_core::test_access::workers(&transport)
}

pub(crate) fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// A synthetic owner without an engine or session log.
pub(crate) struct TestAgentSpec {
    pub(crate) name: String,
    pub(crate) role: Role,
    pub(crate) admission: Admission,
    pub(crate) life: Option<ScopeNode>,
    pub(crate) elapsed: Duration,
    pub(crate) fuel: u32,
    pub(crate) search: bool,
}

impl TestAgentSpec {
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            role: Role::Converses,
            admission: Admission::Free,
            life: None,
            elapsed: Duration::ZERO,
            fuel: 0,
            search: false,
        }
    }

    pub(crate) fn child(name: &str, parent: &AgentHandle, life: ScopeNode) -> Self {
        Self {
            role: Role::Child {
                parent: parent.clone(),
            },
            admission: Admission::Parent {
                idle: crate::agent::fleet::HELPER_IDLE,
            },
            life: Some(life),
            fuel: parent.0.fuel.saturating_sub(1),
            search: parent.0.search,
            ..Self::new(name)
        }
    }
}

/// The mailbox ends the life before the name is released.
pub(crate) struct TestAgent {
    handle: AgentHandle,
    pub(crate) mailbox: AgentMailbox,
    _name: Name,
}

impl std::ops::Deref for TestAgent {
    type Target = AgentHandle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

/// A synthetic mailbox owner using the production name directory.
pub(crate) fn test_agent(fleet: &Arc<Fleet>, spec: TestAgentSpec) -> Result<TestAgent, Unborn> {
    let TestAgentSpec {
        name,
        role,
        admission,
        life,
        elapsed,
        fuel,
        search,
    } = spec;
    let reserved = fleet.registry.name(&name)?;
    let id = crate::agent::fresh_id();
    let started = Instant::now()
        .checked_sub(elapsed)
        .expect("test birth fits the clock");
    let life = life.unwrap_or_else(|| fleet.registry.root.child());
    let recorder = crate::record::Emitter::none();
    recorder.bind(life.clone());
    let mailbox = AgentMailbox::new(
        life,
        recorder,
        admission,
        crate::provider::Pick::of(&scripted("test-model", Script::new())),
    );
    let handle = AgentHandle(Arc::new(Shared {
        id,
        name,
        log_dir: fleet.launch.run_dir().clone(),
        run_dir: fleet.launch.run_dir().clone(),
        started,
        role,
        fuel,
        search,
        address: mailbox.address(),
    }));
    fleet.registry.bind(&reserved, &handle);
    Ok(TestAgent {
        handle,
        mailbox,
        _name: reserved,
    })
}

/// A boundary read through one of `test_access`'s count probes — unlike
/// `scope_has` it ticks no epoch and no ledger.
pub(crate) fn probe_count(
    session: &Agent,
    probe: impl FnOnce(&dyn ral_core::carrier::Transport) -> Result<u64, ral_core::carrier::ProbeError>,
) -> u64 {
    session
        .engine()
        .read(probe)
        .expect("an identity engine never severs")
}

/// Whether `name` resolves, asked through a real eval — which ticks the ral
/// epoch and the binding-lease ledger, so an idle-arithmetic test must count it.
pub(crate) fn scope_has(session: &mut Agent, name: &str) -> bool {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    // Discarded, so a block- or handle-valued binding still settles on data.
    let text = session
        .ral(
            &session.mailbox.life().child(),
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

/// Seed one closed exchange for context and transcript tests.
pub(crate) fn close_exchange(session: &mut Agent, prompt: &str, answer: &str) {
    let log = &mut session.body.log;
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

/// Set a fixture's provider before launch and publish its pick.
pub(crate) fn set_provider(session: &mut Agent, provider: Arc<Provider>) {
    let pick = crate::provider::Pick::of(&provider);
    session.body.state.provider = provider;
    session.mailbox.publish_pick(pick);
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

/// An owned root, not yet launched.
pub(crate) fn trunk(trunk: Trunk) -> Agent {
    Agent::for_test_with(TestTrunk {
        trunk,
        ..TestTrunk::new("system")
    })
    .expect("test trunk")
}

pub(crate) fn host(root: Agent) -> Host {
    Host::adopt(vec![(root, None)]).expect("test host launches its root")
}

/// The toolless, conversing `--chat` root.
pub(crate) fn chat_trunk() -> Agent {
    Agent::for_test_with(TestTrunk {
        trunk: Trunk::Converses,
        tools: Toolset::default(),
        ..TestTrunk::new("system")
    })
    .expect("chat test trunk")
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

/// An unlaunched child carrying the parent's shell snapshot and narrowed grant.
pub(crate) fn forked_child(parent: &Agent, name: &str, grant: &str) -> Agent {
    use crate::agent::engine::{Engine, EngineKind};
    use crate::enquiry::{Agents, ForkClaim, Request, Start};
    use crate::record::Memory;
    use ral_core::data::FOValue;
    use ral_core::data::datum::Datum as _;
    use ral_core::protocol::{EnquiryError, Run};
    use ral_core::source::SourceOrigin;

    struct Capture {
        transport: Arc<ral_core::carrier::IdentityTransport>,
        child: std::sync::Mutex<Option<(Engine, Memory)>>,
    }

    impl ral_core::carrier::Host for Capture {
        fn surface(&self, _val: &FOValue) {}

        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
            let Ok(Request::Agents(Agents::Start(Start {
                spec,
                fork: ForkClaim::Parked(id),
            }))) = Request::decode(&req)
            else {
                panic!("an identity run parks its bequest")
            };
            // Redeem before dispatch drops its unclaimed bequests.
            let engine = self
                .transport
                .adopt_parked(id)
                .map(Engine::adopted)
                .expect("the fork this run parked");
            *self.child.lock().unwrap() = Some((engine, spec.memory));
            Ok(FOValue::Unit)
        }
    }

    let reserved = parent.fleet.registry.name(name).expect("a free child name");
    let EngineKind::Identity(transport) = parent.engine().kind() else {
        panic!("a test parent runs an identity engine")
    };
    let host = Arc::new(Capture {
        transport: transport.clone(),
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
    let (engine, memory) = host
        .child
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("the run never forked a child: {report:?}"));
    Agent::child_of(
        (&parent.handle, parent.mailbox.life().child()),
        &parent.body.log,
        &parent.fleet,
        reserved,
        engine,
        parent.body.state.provider.clone(),
        memory,
    )
    .expect("an adopted child is born")
}

/// Launch a child and put its joined thread in the parent's nursery.
pub(crate) fn launch_child(
    parent: &mut Agent,
    child: Agent,
    first: Option<String>,
    emit: &Emitter,
) -> AgentHandle {
    assert!(
        child
            .handle
            .parent()
            .is_some_and(|handle| handle.id() == parent.id()),
        "a child is launched by its own parent"
    );
    let handle = child.handle();
    let born = Born {
        log_dir: child.log_dir().clone(),
        name: handle.0.name.clone(),
        parent: Some(parent.id()),
        resumed: None,
        reader: child.recorder().reader(),
    };
    let thread = child.launch(born, first, emit).unwrap_or_else(|error| {
        parent.body.state.failures.extend(error.failures);
        panic!(
            "could not launch test child {}: {}",
            handle.id(),
            error.cause
        )
    });
    parent.body.state.nursery.adopt(handle.clone(), thread);
    handle
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
