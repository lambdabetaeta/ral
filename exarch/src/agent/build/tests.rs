use super::*;
use crate::agent::testkit::*;
use crate::bus::{Emitter, Item, Next, Post};
use crate::provider::scripted::Script;
use genai::chat::ChatMessage;
use std::fs;

fn root_config(run_dir: &Path, fuel: u32) -> RootConfig {
    RootConfig {
        system: "system".into(),
        caps: ral_core::capability::GrantStack::root(),
        run_dir: run_dir.to_path_buf(),
        account: RecordedAccount::for_test("test"),
        trunk: Trunk::Attended,
        tools: Toolset::offered(false),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel,
        egress: crate::egress::Egress::for_test(),
        dial: None,
        bureau: Arc::new(crate::provider::Bureau::Scripted),
    }
}

fn identity_seat(tag: &str) -> RootSeat {
    RootSeat::Identity {
        scratch: Arc::new(Scratch::for_test(crate::app::EXARCH, tag).expect("scratch")),
        cwd: std::env::current_dir().expect("test process has a cwd"),
        terminal: ral_core::terminal::TerminalState::default(),
    }
}

/// A forked child inherits the parent's installed builtin surface, not
/// just the core set a bare `HostSurface::default().shell(..)` seeds.
#[test]
fn fork_inherits_host_builtins() {
    let names = |session: &Avatar| {
        session
            .seat
            .read(|t| t.builtin_names())
            .expect("an identity seat never severs")
    };
    let session = Avatar::for_test("system").unwrap();
    assert!(
        names(&session).iter().any(|n| n == "view-text"),
        "the parent boot shell must carry the exarch host builtins"
    );
    let child = names(&session.fork().expect("fork child session"));
    for name in ["view-text", "grep-files", "edit-hash", "explore-dir"] {
        assert!(
            child.iter().any(|n| n == name),
            "the forked child must inherit the host builtin `{name}`"
        );
    }
}

/// Fuel bounds depth, not fan-out: siblings cost the parent nothing, and
/// a chain walked all the way down bottoms out at zero rather than
/// wrapping.
#[test]
fn fork_fans_out_without_spending_the_parents_fuel() {
    let parent = Avatar::for_test("system").unwrap();
    assert_eq!(parent.agent.fuel, SPAWN_FUEL);
    for _ in 0..3 {
        let child = parent.fork().expect("fork child");
        assert_eq!(
            child.agent.fuel,
            SPAWN_FUEL - 1,
            "each child starts one below the parent, regardless of how many siblings it has"
        );
    }
    assert_eq!(
        parent.agent.fuel, SPAWN_FUEL,
        "fork never touches the parent's own fuel: fan-out is unbounded"
    );

    let mut chain = parent;
    for expected in (0..SPAWN_FUEL).rev() {
        chain = chain.fork().expect("fork child");
        assert_eq!(chain.agent.fuel, expected);
    }
    assert_eq!(
        chain.agent.fuel, 0,
        "the chain must bottom out at zero, not wrap"
    );
}

/// A fork carries its parent's search reach verbatim, in both directions
/// — the ceiling the desk's own clamp narrows a spawn against.
#[test]
fn fork_inherits_its_parents_search_reach() {
    let parent = Avatar::for_test("system").unwrap();
    assert!(parent.fork().unwrap().agent.search);
    let searchless = searchless_trunk();
    assert!(
        !searchless.fork().unwrap().agent.search,
        "a searchless parent can hand out no search of its own"
    );
}

/// The provider is per-agent: a later swap on either side never disturbs
/// the other — what `/model` on the focused agent relies on.
#[test]
fn fork_seeds_its_own_provider_handle() {
    let parent = Avatar::for_test("system").unwrap();
    parent.agent.provider.swap(scripted("p-a", Script::new()));
    let child = parent.fork().expect("fork child");
    assert_eq!(
        child.agent.provider.current().model(),
        "p-a",
        "the child seeds its handle from the parent's current provider"
    );
    parent.agent.provider.swap(scripted("p-b", Script::new()));
    assert_eq!(parent.agent.provider.current().model(), "p-b");
    assert_eq!(
        child.agent.provider.current().model(),
        "p-a",
        "a swap on the parent never disturbs an already-forked child"
    );
}

/// A branch imports the creator's context and withholds `reply`, but is
/// otherwise an ordinary fork: caps verbatim, one less fuel.
#[test]
fn branch_imports_context_and_withholds_reply() {
    let parent = Avatar::for_test("system").unwrap();
    parent
        .log
        .borrow_mut()
        .append_user("what did we learn?".into(), None)
        .unwrap();
    parent
        .log
        .borrow_mut()
        .append_assistant(
            genai::chat::ChatMessage::assistant("the invariant matters"),
            vec![],
            None,
        )
        .unwrap();

    let child = parent
        .branch("branch".into(), &crate::bus::dummy_emitter().0)
        .expect("branch child");

    let view = serde_json::to_string(&child.rendered_messages()).unwrap();
    assert!(view.contains("what did we learn?"));
    assert!(view.contains("the invariant matters"));
    assert!(
        view.rfind("the invariant matters") > view.rfind("what did we learn?"),
        "the creator's context is imported in mnemon order: {view}"
    );

    assert!(
        !child.agent.returns,
        "a branch withholds `reply` and never returns"
    );
    assert_eq!(
        child.agent.caps, parent.agent.caps,
        "a branch inherits the creator's capabilities verbatim"
    );
    assert_eq!(
        child.agent.fuel,
        parent.agent.fuel - 1,
        "a branch is a fork: its fuel is one less than the parent's"
    );
}

/// Read `system_prompt_bytes` off a session's `SessionStarted` bookend,
/// the first record in its `record.jsonl`.
fn recorded_system_prompt_bytes(log_dir: &Path) -> usize {
    let records = crate::record::read_records(&log_dir.join("record.jsonl")).unwrap();
    let first = records
        .into_iter()
        .next()
        .expect("record.jsonl must have at least one record");
    match first {
        crate::record::Record::Forensic(crate::record::Forensic::SessionStarted {
            system_prompt_bytes,
            ..
        }) => system_prompt_bytes,
        other => panic!("first record must be SessionStarted, got {other:?}"),
    }
}

/// The bookend records the child's own resolved length — never a
/// parent's, never the raw template's — checked in both directions a
/// `returns` flip can take.
#[test]
fn fork_and_branch_bookend_record_the_childs_own_resolved_length() {
    let dir = tmp("bookend-resolved-length");
    let template = format!(
        "persona\n\n# Builtins\n\n{}",
        crate::prompt::BUILTIN_INDEX_PLACEHOLDER
    );
    let root = Avatar::root(
        RootConfig {
            system: template,
            ..root_config(dir.path(), SPAWN_FUEL)
        },
        identity_seat("bookend-resolved-length"),
        scripted("test-model", Script::new()),
    )
    .expect("root trunk");

    let child = root.fork().expect("fork child");
    assert_ne!(
        child.agent.system.len(),
        root.agent.system.len(),
        "the fork's bits must actually differ from its parent's for \
         this test to be meaningful"
    );
    assert_eq!(
        recorded_system_prompt_bytes(&child.log_dir()),
        child.agent.system.len(),
        "an ordinary fork's bookend must record its own resolved \
         system, not its non-returning parent's"
    );

    let grandchild = child
        .branch("grandchild".into(), &crate::bus::dummy_emitter().0)
        .expect("branch grandchild");
    assert_ne!(
        grandchild.agent.system.len(),
        child.agent.system.len(),
        "the branch's bits must actually differ from its parent's for \
         this test to be meaningful"
    );
    assert_eq!(
        recorded_system_prompt_bytes(&grandchild.log_dir()),
        grandchild.agent.system.len(),
        "a /branch child's bookend must record its own resolved \
         system, not its returning parent's"
    );
}

/// `/clear` cancels every registered worker, the durable class included,
/// and the rebuilt shell starts empty.  A worker settling *after* the
/// clear still flushes its batch to the inbox, stamped with its birth
/// epoch, and the inbox's own pop is the edge that rejects it.  The
/// workers stay deaf until `CLEAR_RELEASE` so they settle past the inbox
/// drop; settling inside the clear would have `Inbox::clear` eat the
/// batch instead, leaving this straggler path unexercised.
#[test]
fn clear_cancels_registered_workers_and_drops_their_late_surface() {
    let mut session = dressed_trunk(|shell| shell.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));

    // The deferred sink `Avatar::ral` wires captures `emit`'s mailbox, which
    // must be this session's own inbox for the late-surface assertion
    // below to mean anything.
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
    let _ = session.ral("spawn { test-clear-block-until-released }", 30, &emit);
    let _ = session.ral(
        r#"service "clear-test" { test-clear-block-until-released }"#,
        30,
        &emit,
    );

    let entries = workers(&session);
    assert_eq!(entries.len(), 2, "one ordinary worker, one service");
    let durable = entries
        .iter()
        .find(|e| e.class == ral_core::types::LeaseClass::Durable)
        .expect("the service must register under the durable class");
    assert_eq!(durable.cmd, "clear-test");
    for entry in &entries {
        assert!(
            !entry.handle.cancel.is_cancelled(),
            "freshly spawned, not yet touched by /clear"
        );
    }

    session.clear().expect("clear must succeed");

    for entry in &entries {
        assert!(
            entry.handle.cancel.is_cancelled(),
            "/clear must cancel every registered worker, the durable class included ({})",
            entry.cmd
        );
    }
    assert_eq!(
        probe_count(&session, ral_core::test_access::worker_count),
        0,
        "the rebuilt shell's registry must start empty"
    );

    // The clear has fully returned — its inbox drop is behind us — so the
    // workers may now observe cancellation and flush.  Generous budget:
    // the suite runs oversubscribed in a VM.
    CLEAR_RELEASE.store(true, std::sync::atomic::Ordering::Release);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    for entry in &entries {
        loop {
            if entry.handle.state() != ral_core::types::HandleState::Running {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the cancelled worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    assert!(
        !session.inbox.is_empty(),
        "the settling worker still posts its late batch: the sink never withholds"
    );
    assert!(
        session.inbox.next_item().is_none(),
        "the late batch's birth epoch must be rejected at the inbox's own pop"
    );
}

/// The `/clear` gesture end to end for an agent child: a worker born
/// under the old context reports after the clear, its result reaches the
/// inbox stamped with the consumer epoch of that old context, and the
/// inbox's own pop refuses it.
#[test]
fn late_child_result_after_clear_is_refused_at_the_pop() {
    let mut session = Avatar::for_test("system").unwrap();
    let mut spec = TestAgentSpec::new("worker");
    spec.parent = Some(session.agent.clone());
    let worker = test_agent(&session.fleet, spec).expect("a fresh child of a live session");

    session.clear().expect("clear must succeed");
    worker.report(crate::bus::AgentOutcome::Stopped("late".into()));

    assert!(
        !session.inbox.is_empty(),
        "the late result is posted, never withheld: deliver-then-retire is structural"
    );
    assert!(
        session.inbox.next_item().is_none(),
        "and refused at the inbox's own pop"
    );
}

/// The fence is addressed, not global: a `/clear` in one tab must leave
/// a result another tab is still waiting on untouched.
#[test]
fn a_clear_in_one_tab_leaves_another_tabs_late_result_alone() {
    let trunk = Avatar::for_test("system").unwrap();
    let mut branch = Avatar::for_test("system").unwrap();
    let mut spec = TestAgentSpec::new("worker");
    spec.parent = Some(trunk.agent.clone());
    let worker = test_agent(&trunk.fleet, spec).expect("a fresh child of the trunk");

    branch.clear().expect("clear must succeed");
    worker.report(crate::bus::AgentOutcome::Stopped("done".into()));

    assert!(
        matches!(trunk.inbox.next_item(), Some(Next::Item(Item::Agent(_)))),
        "the branch's /clear must not poison the trunk's delivery"
    );
}

/// An agent that ends without ever being cancelled — the ordinary settle
/// at the end of its `attend` — has no cascade edge pointed at it, so
/// `Drop` is the only thing that reaches its workers.
#[test]
fn agent_drop_cancels_its_own_unclosed_workers() {
    let avatar = dressed_trunk(|shell| shell.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));

    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, avatar.agent.id, avatar.inbox.mailbox());
    let _ = avatar.ral("spawn { test-clear-block-forever }", 30, &emit);

    let entries = workers(&avatar);
    assert_eq!(entries.len(), 1, "the agent's own spawn must register");
    assert!(!entries[0].handle.cancel.is_cancelled(), "freshly spawned");

    drop(avatar);

    assert!(
        entries[0].handle.cancel.is_cancelled(),
        "dropping the agent (settle or cancel: however its life ended) \
         must cancel its own still-running workers"
    );
}

/// A self-armed wakeup re-arms on the shared reaper for as long as its
/// guard lives, and the registry is `Arc`-shared with the reaper's
/// closure, so it outlives a bare drop of the `Agent`: without `Drop`'s
/// clear, a settled agent's cron fires into an inbox nobody drains.
#[test]
fn agent_drop_clears_its_own_armed_schedules() {
    let agent = Avatar::for_test("system").unwrap();
    let schedules = agent.agent.schedules.clone();
    schedules
        .schedule(
            crate::schedule::Trigger::After(std::time::Duration::from_hours(1)),
            "ping".into(),
            "ping".into(),
            &agent.inbox.mailbox(),
        )
        .expect("a one-hour `after` trigger must arm");
    assert_eq!(schedules.list().len(), 1, "the schedule is armed");

    drop(agent);

    assert!(
        schedules.list().is_empty(),
        "dropping the agent must clear its own armed schedules, the same \
         law /clear already applies explicitly"
    );
}

/// `/clear` re-arms over a freshly booted shell, so the fresh boot's scope
/// is the new baseline and the pre-clear binding is gone with the whole
/// old `Shell`, ledger included.
#[test]
fn clear_reseals_baseline_and_forgets_ledger() {
    let mut session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.ral("let pre_clear_x = 1", 5, &emit);

    session.clear().expect("clear must succeed");

    assert!(
        !scope_has(&session, "pre_clear_x"),
        "the pre-clear binding must not survive the rebuild"
    );
}

#[test]
fn rewind_validates_the_anchor_and_sheds_nudges() {
    let mut session = Avatar::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        for (prompt, answer) in [
            ("one", "answer one"),
            ("two", "answer two"),
            ("three", "answer three"),
        ] {
            log.append_user(prompt.into(), None).unwrap();
            log.append_assistant(ChatMessage::assistant(answer), Vec::new(), None)
                .unwrap();
        }
    }
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);

    assert_eq!(
        session.rewind(9, &emit).unwrap_err(),
        "turn 9 is not recorded: the latest is 6"
    );

    session.inbox.push(Post::Nudge {
        prompt: 5,
        text: "stale continuation".into(),
    });
    session
        .rewind(3, &emit)
        .expect("an anchor still in context is legal");
    assert_eq!(
        session
            .log
            .borrow_mut()
            .context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the rewind removes the anchor and everything after it"
    );
    assert!(
        !matches!(
            session.inbox.next_item(),
            Some(Next::Item(Item::Nudge { .. }))
        ),
        "a queued nudge for a rewound prompt must not commit"
    );
    let records = crate::bus::drain_records(&rx);
    assert!(
        records.iter().any(|record| matches!(
            record,
            crate::record::Record::Protocol(crate::record::Protocol::Rewound { anchor: 3 })
        )),
        "rewind must be durable on the trace"
    );
    assert!(
        records.iter().any(|record| matches!(
            record,
            crate::record::Record::Display(crate::record::Display::Rewound { anchor: 3 })
        )),
        "and drawn"
    );
}

/// A fork is armed over the whole scope it snapshotted, sealing parent
/// scratch included as its baseline: a name
/// the parent leased is never a lease candidate in the child, however
/// many idle calls it runs.
#[test]
fn fork_child_inherited_scratch_is_baseline() {
    let session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.ral("let parent_scratch = 1", 5, &emit);

    let child = session.fork().expect("fork");
    assert!(
        scope_has(&child, "parent_scratch"),
        "a fork snapshots the parent's whole scope"
    );

    // Against the production bound the fork was armed with.
    let (child_tx, _child_rx) = crate::bus::channel();
    let child_emit = Emitter::new(child_tx, child.agent.id);
    for _ in 0..(crate::shell_eval::BINDING_IDLE_CALLS + 5) {
        child.ral("let _child_spin = 0", 5, &child_emit);
    }
    assert!(
        scope_has(&child, "parent_scratch"),
        "inherited parent scratch is baseline in the child: never pruned, however \
         many boundary prunes the idle calls above ran"
    );
}

#[test]
fn resumed_agent_adds_only_the_fresh_shell_note() {
    let dir = tmp("resume-agent");
    let sessions = dir.path().join("sessions");
    let mut log = AgentLog::root(
        &sessions,
        AgentId::new(0),
        &RecordedModel::for_test("old-model"),
        &RecordedAccount::for_test("old-provider"),
        0,
    )
    .unwrap();
    log.append_user("before the crash".into(), None).unwrap();
    log.append_assistant(ChatMessage::assistant("saved answer"), vec![], None)
        .unwrap();
    let before = log.context().rendered();
    drop(log);

    let (agent, resumed) = Avatar::resume(
        RootConfig {
            account: RecordedAccount::for_test("new-provider"),
            ..root_config(dir.path(), 0)
        },
        identity_seat("resume-agent"),
        scripted("new-model", Script::new()),
    )
    .expect("resumed agent");

    assert_eq!(resumed.turn, 2, "the summary is taken before the note");
    assert!(agent.is_ready());
    let messages = agent.rendered_messages();
    assert_eq!(
        serde_json::to_vec(&messages[..before.len()]).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );
    assert_eq!(messages.len(), before.len() + 1);
    let note = messages.last().expect("fresh-shell note");
    assert_eq!(note.role, genai::chat::ChatRole::User);
    let text = note.content.first_text().expect("note text");
    for loss in [
        "shell is fresh",
        "bindings",
        "workers",
        "cwd",
        "scratch",
        "pinned state",
        "scheduled events",
        "sub-agents",
    ] {
        assert!(text.contains(loss), "resume note must name {loss}: {text}");
    }
    let records = crate::record::read_records(&dir.path().join("sessions/0/record.jsonl")).unwrap();
    assert!(records.iter().any(|record| matches!(
        record,
        crate::record::Record::Forensic(crate::record::Forensic::SessionResumed { .. })
    )));
}

/// The rotation swaps the segment behind the recorder, never the
/// recorder itself: one handed out before the clear, and the bus coupled
/// before it, both keep publishing afterwards.  Swapping the recorder
/// instead left the frontend dark for the whole first exchange of the
/// cleared session.
#[test]
fn clear_rotates_record_jsonl_and_shared_emitters_follow_the_new_segment() {
    let mut session = Avatar::for_test("system").unwrap();
    let record = session.log_dir().join("record.jsonl");
    fs::write(record.with_file_name("record.jsonl.0"), b"reserved").unwrap();
    let (tx, rx) = crate::bus::channel();
    session.couple(&Emitter::new(tx, session.agent.id));
    let recorder = session.recorder();

    session.clear().expect("clear rotation");

    let rotated_record = record.with_file_name("record.jsonl.1");
    assert!(rotated_record.is_file());
    assert!(record.is_file());
    assert!(
        fs::read(record.with_file_name("record.jsonl.0"))
            .unwrap()
            .starts_with(b"reserved")
    );

    let _recorded = recorder
        .emit(crate::record::Display::Prompt {
            text: "after the clear".into(),
            turn: Some(1),
        })
        .expect("the pre-clear recorder still appends");
    recorder.transient(crate::record::Transient::Cleared);

    let current_records = crate::record::read_records(&record).unwrap();
    assert!(matches!(
        current_records.first().expect("new session head"),
        crate::record::Record::Forensic(crate::record::Forensic::SessionStarted { .. })
    ));
    assert!(
        current_records.iter().any(|r| matches!(
            r,
            crate::record::Record::Display(crate::record::Display::Prompt { text, .. })
                if text == "after the clear"
        )),
        "the pre-clear recorder must write into the new segment, not the rotated one"
    );

    let published: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        published.iter().any(|s| matches!(
            s,
            crate::bus::Signal::Fact(_, rec)
                if matches!(rec.value(), crate::record::Record::Display(
                    crate::record::Display::Prompt { text, .. }) if text == "after the clear")
        )),
        "and the bus coupled before the clear must still see it"
    );
    assert!(
        published.iter().any(|s| matches!(
            s,
            crate::bus::Signal::Transient(_, crate::record::Transient::Cleared)
        )),
        "including the `Cleared` acknowledgement the frontend waits on"
    );
}

#[test]
fn resume_seeds_child_ids_past_existing_session_directories() {
    let dir = tmp("resume-id-seed");
    let sessions = dir.path().join("sessions");
    let log = AgentLog::root(
        &sessions,
        AgentId::new(0),
        &RecordedModel::for_test("old-model"),
        &RecordedAccount::for_test("old-provider"),
        0,
    )
    .unwrap();
    fs::create_dir_all(sessions.join("1")).unwrap();
    fs::write(sessions.join("1/sentinel"), b"keep").unwrap();
    drop(log);

    let (root, _) = Avatar::resume(
        RootConfig {
            account: RecordedAccount::for_test("new-provider"),
            ..root_config(dir.path(), 1)
        },
        identity_seat("resume-id-seed"),
        scripted("new-model", Script::new()),
    )
    .expect("resumed root");
    let child = root.fork().expect("post-resume child");
    let child_id = child
        .log_dir()
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<u64>().ok())
        .expect("numeric child session directory");
    assert!(child_id >= 2);
    assert_eq!(fs::read(sessions.join("1/sentinel")).unwrap(), b"keep");
}
