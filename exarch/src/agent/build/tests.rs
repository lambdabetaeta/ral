use super::*;
use crate::agent::testkit::*;
use crate::bus::Emitter;
use crate::provider::scripted::Script;
use crate::record::{AgentId, AgentLog, Memory, RECORD, Selected, Voice};
use genai::chat::ChatMessage;
use ral_core::process::DurableRoot;
use std::fs;

mod clear;
mod resume;

fn launch_config(run_dir: &OwnDir, fuel: u32) -> LaunchConfig {
    LaunchConfig {
        system: "system".into(),
        run_dir: run_dir.clone(),
        returns: false,
        admission: Admission::Free,
        helper_idle: HELPER_IDLE,
        max_tokens: None,
        tools: Toolset::offered(false),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel,
        search: true,
        factory: None,
        bureau: Arc::new(crate::provider::Bureau::scripted([])),
        roles: crate::provider::roles::Roles::default(),
    }
}

fn identity_engine(tag: &str) -> RootEngine {
    RootEngine::Identity {
        scratch: Arc::new(Scratch::for_test(crate::app::EXARCH, tag).expect("scratch")),
        cwd: ral_core::host::cwd().expect("test process has a cwd"),
        grants: GrantStack::default(),
        terminal: ral_core::io::terminal::TerminalState::default(),
    }
}

#[test]
fn parent_terms_lend_launch_policy_or_helper_policy_and_saturating_fuel() {
    use ral_core::process::CancelCause;
    use std::time::Duration;

    for returns in [false, true] {
        let fleet = Fleet::new(Launch {
            fuel: 19,
            search: false,
            admission: Admission::Gated,
            returns,
            idle: Duration::from_secs(23),
            ..Launch::for_test()
        });
        let human = Parent::Human.terms(&fleet.launch, &fleet.registry);
        assert!(human.parent.is_none());
        assert_eq!(human.fuel, 19);
        assert!(!human.search);
        assert_eq!(human.admission, Admission::Gated);
        assert_eq!(human.returns, returns);
        assert!(!human.life.is_cancelled());

        for (fuel, expected) in [(0, 0), (4, 3)] {
            let parent = test_agent(
                &fleet,
                TestAgentSpec {
                    fuel,
                    search: true,
                    ..TestAgentSpec::new("parent")
                },
            )
            .unwrap();
            let life = DurableRoot::default().child();
            let helper = Parent::Agent(&parent, life.clone()).terms(&fleet.launch, &fleet.registry);
            assert_eq!(
                helper.parent.as_ref().map(AgentHandle::id),
                Some(parent.id())
            );
            assert_eq!(helper.fuel, expected);
            assert!(helper.search);
            assert_eq!(
                helper.admission,
                Admission::Parent {
                    idle: fleet.launch.idle
                }
            );
            assert!(helper.returns);
            assert!(!helper.life.is_cancelled());
            life.cancel(CancelCause::Interrupted);
            assert_eq!(helper.life.cause(), Some(CancelCause::Interrupted));
            assert!(parent.is_live());
        }
        fleet.registry.root.cancel(CancelCause::Cancelled);
        assert_eq!(human.life.cause(), Some(CancelCause::Cancelled));
    }
}

#[test]
fn fork_inherits_host_builtins() {
    let names = |session: &Agent| session.engine().read(|t| t.builtin_names()).unwrap();
    let session = Agent::for_test("system").unwrap();
    assert!(names(&session).iter().any(|n| n == "view-text"));
    let child = names(&session.fork().expect("fork child"));
    for name in ["view-text", "grep-files", "edit-hash", "explore-dir"] {
        assert!(
            child.iter().any(|n| n == name),
            "the child inherits `{name}`"
        );
    }
}

#[test]
fn fork_fans_out_without_spending_the_parents_fuel() {
    let parent = Agent::for_test("system").unwrap();
    assert_eq!(parent.handle.0.fuel, SPAWN_FUEL);
    for _ in 0..3 {
        let child = parent.fork().unwrap();
        assert_eq!(child.handle.0.fuel, SPAWN_FUEL - 1);
    }
    assert_eq!(parent.handle.0.fuel, SPAWN_FUEL);
    let mut chain = vec![parent];
    for expected in (0..SPAWN_FUEL).rev() {
        let child = chain.last().unwrap().fork().unwrap();
        assert_eq!(child.handle.0.fuel, expected);
        chain.push(child);
    }
    assert_eq!(chain.last().unwrap().handle.0.fuel, 0);
}

#[test]
fn fork_inherits_its_parents_search_reach() {
    let parent = Agent::for_test("system").unwrap();
    assert!(parent.fork().unwrap().handle.0.search);
    let searchless = searchless_trunk();
    assert!(!searchless.fork().unwrap().handle.0.search);
}

#[test]
fn fork_inherits_its_provider_independently_of_later_parent_changes() {
    let mut parent = Agent::for_test("system").unwrap();
    set_provider(&mut parent, scripted("p-a", Script::new()));
    let child = parent.fork().unwrap();
    assert_eq!(
        child
            .mailbox
            .transact(|txn| txn.model().name().to_string())
            .unwrap(),
        "p-a"
    );
    set_provider(&mut parent, scripted("p-b", Script::new()));
    assert_eq!(
        parent
            .mailbox
            .transact(|txn| txn.model().name().to_string())
            .unwrap(),
        "p-b"
    );
    assert_eq!(
        child
            .mailbox
            .transact(|txn| txn.model().name().to_string())
            .unwrap(),
        "p-a"
    );
}

#[test]
fn mnemon_root_imports_context_withholds_reply_and_resumes() {
    let mut parent = trunk(false);
    {
        let log = &mut parent.body.log;
        log.append_user("what did we learn?".into(), None, Voice::Human)
            .unwrap();
        log.append_assistant(
            ChatMessage::assistant("the invariant matters"),
            vec![],
            None,
        )
        .unwrap();
    }
    let child =
        crate::agent::testkit::human_child(&parent, Memory::Mnemon, Some("fork".into())).unwrap();
    let view = serde_json::to_string(&child.rendered_messages()).unwrap();
    assert!(view.contains("what did we learn?"));
    assert!(view.contains("the invariant matters"));
    assert!(view.rfind("the invariant matters") > view.rfind("what did we learn?"));
    assert!(
        !child
            .handle
            .0
            .address
            .transact(|txn| txn.mail().returns())
            .unwrap()
    );
    assert!(child.handle.parent().is_none());
    assert_eq!(
        child.engine().read(|t| t.grants()).unwrap(),
        parent.engine().read(|t| t.grants()).unwrap()
    );
    assert_eq!(child.handle.0.fuel, parent.handle.0.fuel);
    let sessions = parent.fleet.launch.sessions_root.clone();
    let live = AgentLog::human_children(&sessions).unwrap();
    let names: Vec<_> = live.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["main", "fork"]);
    let id = child.id();
    let before = child.rendered_messages();
    drop(child);
    let (resumed, _, _, _) = AgentLog::reopen(
        &sessions,
        id,
        parent.body.client.selected(),
        false,
        parent.body.log.context().system(),
        crate::app::now_unix_ms(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(&resumed.context().rendered()).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );
}

#[test]
fn a_closed_root_is_not_resumable() {
    let mut session = Agent::for_test("system").unwrap();
    let sessions = session.fleet.launch.sessions_root.clone();
    assert_eq!(AgentLog::human_children(&sessions).unwrap().len(), 1);
    let (emit, _rx) = crate::bus::dummy_emitter();
    assert!(
        session
            .rewrite(&crate::bus::Rewrite::Close, &emit)
            .is_break()
    );
    assert_eq!(AgentLog::human_children(&sessions).unwrap(), vec![]);
}

#[test]
fn a_reborn_engine_carries_the_life() {
    use ral_core::carrier::{Severed, Transport as _};
    let mut session = Agent::for_test("system").unwrap();
    let crate::agent::engine::EngineKind::Identity(outgoing) = session.engine().kind() else {
        panic!("an identity root");
    };
    session.clear().unwrap();
    assert!(session.handle().is_live());
    outgoing.sever(Severed::Closed("the outgoing engine".into()));
    assert!(
        session.handle().is_live(),
        "the outgoing engine no longer holds the life"
    );
    session
        .engine()
        .transport()
        .sever(Severed::Closed("the reborn engine".into()));
    assert!(!session.handle().is_live());
}

#[test]
fn agent_drop_cancels_its_own_unclosed_workers() {
    let mut agent =
        dressed_trunk(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, agent.id());
    agent.ral(
        &DurableRoot::default().child(),
        "spawn \"blocker\" { test-clear-block-forever }",
        30,
        &emit,
    );
    let entries = workers(&agent);
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].handle.cancel.is_cancelled());
    drop(agent);
    assert!(entries[0].handle.cancel.is_cancelled());
}

#[test]
fn agent_drop_clears_its_own_armed_schedules() {
    let mut agent = Agent::for_test("system").unwrap();
    crate::agent::testkit::schedule(
        &mut agent,
        crate::schedule::Trigger::After(std::time::Duration::from_hours(1)),
        "ping".into(),
        "ping".into(),
    );
    assert_eq!(crate::agent::testkit::schedules(&agent).len(), 1);
    let handle = agent.handle();
    drop(agent);
    assert!(!handle.is_live());
}

#[test]
fn clear_reseals_baseline_and_forgets_ledger() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        "let pre_clear_x = 1",
        5,
        &emit,
    );
    session.clear().unwrap();
    assert!(!scope_has(&mut session, "pre_clear_x"));
}

#[test]
fn fork_child_inherited_scratch_is_baseline() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        "let parent_scratch = 1",
        5,
        &emit,
    );
    let mut child = forked_child(&session, "child", "`inherit");
    assert!(scope_has(&mut child, "parent_scratch"));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    for _ in 0..(crate::shell_eval::BINDING_IDLE_CALLS + 5) {
        child.ral(
            &DurableRoot::default().child(),
            "let _child_spin = 0",
            5,
            &emit,
        );
    }
    assert!(scope_has(&mut child, "parent_scratch"));
}

#[test]
fn a_root_runs_under_its_launch_grants() {
    use ral_core::authority::policy::{Capabilities, FsPolicy};
    use ral_core::path::FrozenPath;
    let cwd = ral_core::host::cwd().unwrap();
    let grants = ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
        shell: Some(std::collections::BTreeSet::new()),
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(cwd.as_path()).unwrap()],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    })]);
    let mut session = Agent::for_test_with(TestLaunch {
        grants,
        ..TestLaunch::new("system")
    })
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("file"), "hidden").unwrap();
    let check = |session: &mut Agent| {
        assert_eq!(
            session
                .engine()
                .read(|t| t.path_entries(tmp.path()))
                .unwrap(),
            [] as [ral_core::protocol::probe::PathEntry; 0]
        );
        let (emit, _rx) = crate::bus::dummy_emitter();
        for _ in 0..2 {
            let result = session.ral(&DurableRoot::default().child(), "cd .", 5, &emit);
            assert!(
                result.failed && result.text.contains("denied"),
                "{}",
                result.text
            );
        }
        let result = session.ral(&DurableRoot::default().child(), "$[1 + 1]", 5, &emit);
        assert!(
            !result.failed && result.text.contains("VALUE:\n2"),
            "{}",
            result.text
        );
    };
    check(&mut session);
    session.clear().unwrap();
    check(&mut session);
    let mut root =
        crate::agent::testkit::human_child(&session, Memory::Amnemon, Some("new-root".into()))
            .unwrap();
    check(&mut root);
}

#[test]
fn a_child_gets_parent_meet_narrowing_without_replay_at_any_depth() {
    use ral_core::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
    use ral_core::path::{FrozenPath, shown};
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::write(&a, "a").unwrap();
    fs::write(&b, "child-readable-b").unwrap();
    let cwd = ral_core::host::cwd().unwrap();
    let here = FrozenPath::freeze(cwd.as_path()).unwrap();
    let a_path = FrozenPath::freeze(&a).unwrap();
    let b_path = FrozenPath::freeze(&b).unwrap();
    let runtime = crate::shell_eval::builtins::host_surface()
        .runtime(ral_core::io::terminal::TerminalState::default())
        .unwrap();
    let explicit_reads =
        ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![here.clone()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        })]);
    for path in [&a, &b] {
        assert!(
            !explicit_reads.admits_fs(
                runtime.baseline(),
                &ral_core::authority::policy::FsOp::Read,
                &ral_core::test_helper::trace(path)
            ),
            "{}",
            shown(path)
        );
    }
    let mut grants = ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
        exec: Some(ExecGrant::of(
            [ExecKey::Path(a_path.clone()), ExecKey::Path(b_path.clone())],
            [],
        )),
        fs: Some(FsPolicy {
            read_prefixes: vec![here.clone()],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    })]);
    let parent = Agent::for_test_with(TestLaunch {
        grants: grants.clone(),
        ..TestLaunch::new("system")
    })
    .unwrap();
    assert_eq!(
        parent.engine().read(|t| t.grants()).unwrap(),
        grants.summary()
    );
    let c = format!(
        "`restrict [exec: [allow: [`absolute '{}']], fs: [read: [`cwd ., `absolute '{}']]]",
        shown(&a),
        shown(&b)
    );
    let mut child = forked_child(&parent, "child", &c);
    grants.push(ral_core::test_helper::layer(Capabilities {
        exec: Some(ExecGrant::of([ExecKey::Path(a_path)], [])),
        fs: Some(FsPolicy {
            read_prefixes: vec![here, b_path],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    }));
    assert_eq!(
        child.engine().read(|t| t.grants()).unwrap(),
        grants.summary()
    );
    let (emit, _rx) = crate::bus::dummy_emitter();
    let result = child.ral(
        &DurableRoot::default().child(),
        &format!("view-text '{}' 1 2", shown(&b)),
        5,
        &emit,
    );
    assert!(
        !result.failed && result.text.contains("child-readable-b"),
        "{}",
        result.text
    );
    let grandchild = forked_child(&child, "grandchild", "`restrict [shell: []]");
    grants.push(ral_core::test_helper::layer(Capabilities {
        shell: Some(std::collections::BTreeSet::new()),
        ..Capabilities::default()
    }));
    assert_eq!(
        grandchild.engine().read(|t| t.grants()).unwrap(),
        grants.summary()
    );
}

#[test]
fn unwind_ends_non_owing_agents_with_schedules() {
    let mut agent = trunk(false);
    crate::agent::testkit::schedule(
        &mut agent,
        crate::schedule::Trigger::After(std::time::Duration::from_hours(1)),
        "ping".into(),
        "ping".into(),
    );
    let handle = agent.handle();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _agent = agent;
        panic!("unwind the owner");
    }));
    assert!(unwind.is_err());
    assert!(!handle.is_live());
}
