use super::*;
use crate::agent::testkit::{TestAgent, TestAgentSpec, test_agent};
use crate::bus::Signal;
use crate::card::Mark;
use crate::record::AgentState;
use crate::tui::palette::READ_W;
use crate::tui::row::Row;
use crate::tui::status::StateSpan;
use ral_core::shell::runtime::{Observation, Observed};

fn prompt(vi: bool) -> PromptState {
    PromptState::new(vi, crate::tui::prompt::History::default())
}

/// The root is returned alongside its `App` because the frontend resolves
/// it through the fleet only: dropping it here would settle the agent mid-test.
pub(in crate::tui) fn app() -> (App, BusReceiver, TestAgent) {
    app_born(&Born::for_test("main", None))
}

/// [`app`] over a root announced as `born`.
pub(in crate::tui) fn app_born(born: &Born) -> (App, BusReceiver, TestAgent) {
    let (app, rx, root, _fleet) = app_in_fleet(born);
    (app, rx, root)
}

/// [`app_born`], with the fleet a test adds helpers to.
fn app_in_fleet(born: &Born) -> (App, BusReceiver, TestAgent, Arc<Fleet>) {
    let (_tx, rx) = crate::bus::channel();
    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh root");
    let mut app = App::new(
        root.id(),
        born,
        fleet.clone(),
        UsageMeter::default(),
        prompt(false),
        Card(Vec::new()),
        mpsc::channel().0,
    );
    app.copy_sink = Some(Box::new(|_| Ok(clipboard::Copied::Whole)));
    (app, rx, root, fleet)
}

const REST: crate::bus::Rest = crate::bus::Rest {
    pending: false,
    schedules: false,
};

fn job_commit(app: &mut App, id: AgentId, number: u64, fact: crate::record::Forensic) {
    app.commit(
        id,
        &Commit::placeholder(
            number,
            crate::record::NonEmpty::one(crate::record::Record::Forensic(fact)),
        ),
    );
}

/// Keep the exchange lease alive while the UI addresses its recorded job.
fn open_exchange(app: &mut App, root: &TestAgent) -> crate::bus::Lease {
    root.steer("busy".into()).unwrap();
    let crate::bus::Next::Take { exchange, .. } = root.mailbox.next(REST) else {
        panic!("the queued prompt starts an exchange");
    };
    assert!(root.mailbox.begin());
    let job = root.status().job.expect("begin records a job");
    job_commit(
        app,
        root.id(),
        job.get(),
        crate::record::Forensic::JobOpened,
    );
    exchange
}

fn apply_signal(app: &mut App, rx: &BusReceiver, signal: Signal) {
    match signal {
        Signal::Born(id, born) => app.born(id, &born),
        Signal::Commit(id, commit) => app.commit(id, &commit),
        Signal::Transient(id, transient) => app.transient(id, transient, rx),
    }
}

/// A scripted bureau and a bare session: the handles a command is serviced against.
pub(in crate::tui) fn ctx() -> CommandCtx<'static> {
    static BUREAU: Bureau = Bureau::Scripted;
    static CWD: std::sync::LazyLock<ral_core::path::CwdPath> =
        std::sync::LazyLock::new(|| ral_core::host::cwd().expect("test process has a cwd"));
    static INFO: std::sync::LazyLock<SessionInfo<'static>> =
        std::sync::LazyLock::new(|| SessionInfo {
            system_size: 0,
            system_files: &[],
            base: "",
            extend: None,
            restrict_files: &[],
            cwd: &CWD,
        });
    CommandCtx {
        bureau: &BUREAU,
        info: &INFO,
    }
}

fn born(app: &mut App, id: AgentId, name: &str, parent: AgentId) {
    app.born(id, &Born::for_test(name, Some(parent)));
}

pub(in crate::tui) fn text(app: &mut App, id: AgentId) -> String {
    let w = app
        .tabs
        .scrollback_mut(id)
        .expect("the tab under test has a scrollback")
        .render_window(READ_W, 40);
    w.lines
        .iter()
        .map(Row::plain)
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_fault_visible(app: &mut App, id: AgentId, fault: &str) {
    let compact = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    for width in [48, READ_W, 160] {
        let window = app
            .tabs
            .scrollback_mut(id)
            .unwrap()
            .render_window(width, 40);
        let visible = window.lines.iter().map(Row::plain).collect::<String>();
        let text = compact(&visible);
        let fault = compact(fault);
        let from = text.find(&fault).unwrap_or_else(|| panic!("{visible}"));
        let until = from + fault.len();
        let mut offset = 0;
        // The opening banner has its own fixed width.
        for row in &window.lines {
            let start = offset;
            offset += compact(&row.plain()).len();
            if start < until && from < offset {
                assert!(
                    row.width() <= usize::from(width) + crate::tui::palette::RAIL_W,
                    "content budget {width}, diagnostic row width {}: {:?}",
                    row.width(),
                    row.plain()
                );
            }
        }
    }
}

#[test]
fn recording_failure_ends_only_its_tab_and_leaves_an_independent_root_usable() {
    for poison in [false, true] {
        recording_failure_with_independent_root(poison);
    }
}

fn recording_failure_with_independent_root(poison: bool) {
    use crate::agent::Trunk;
    use crate::agent::outcome::Outcome;
    use crate::agent::spawn::hatch_root;
    use crate::agent::testkit::{scripted, trunk};
    use crate::bus::FleetBus;
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{Memory, Voice};
    use ral_core::process::DurableRoot;

    let root = trunk(Trunk::Converses);
    let id = root.id();
    let fleet = root.fleet.clone();
    let mut other = hatch_root(&root, Memory::Amnemon, Some("independent".into()))
        .expect("an independent root");
    let bus = FleetBus::session();
    let emit = bus.emitter(id);
    let born = Born {
        log_dir: root.log_dir().clone(),
        name: "main".into(),
        parent: None,
        resumed: None,
        recorder: root.recorder(),
    };
    let other_born = Born {
        log_dir: other.log_dir().clone(),
        name: other.handle().0.name.clone(),
        parent: None,
        resumed: None,
        recorder: other.recorder(),
    };
    if poison {
        root.recorder().poison_append();
    } else {
        root.recorder().fail_after(7);
    }
    let thread = root
        .launch(born, Some("must be recorded before sending".into()), &emit)
        .expect("the owner thread starts");
    let rx = bus.rx();
    let Signal::Born(born_id, born) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("birth precedes the owner's output");
    };
    assert_eq!(born_id, id);
    let mut app = App::new(
        id,
        &born,
        fleet.clone(),
        bus.meter(),
        prompt(false),
        Card(Vec::new()),
        mpsc::channel().0,
    );
    app.born(other.id(), &other_born);
    let mut faults = 0;
    let mut fault = None;
    loop {
        let signal = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the failed owner ends");
        let died =
            matches!(&signal, Signal::Transient(agent, Transient::Died { .. }) if *agent == id);
        if let Signal::Transient(agent, Transient::Fault { text }) = &signal
            && *agent == id
        {
            faults += 1;
            fault = Some(text.clone());
        }
        if died {
            assert!(
                fleet.resolve("main").is_none(),
                "the name leaves before Died"
            );
            assert_fault_visible(&mut app, id, fault.as_deref().expect("fault precedes Died"));
        }
        apply_signal(&mut app, rx, signal);
        if died {
            break;
        }
    }
    thread.join().unwrap();
    assert!(born.recorder.check_available().is_err());
    assert_eq!(faults, 1);
    assert_fault_visible(&mut app, id, &fault.unwrap());
    assert!(matches!(
        app.tabs.status(id).unwrap().state,
        StateSpan::Ended
    ));
    assert!(app.tabs.lingering(id));
    assert_eq!(app.tabs.focused(), other.id());
    assert!(other.handle().is_live() && app.is_steerable());

    let provider = scripted(
        "test-model",
        Script::new().then(Reply::text("still usable")),
    );
    assert!(matches!(
        other.deliberate(
            &DurableRoot::default().child(),
            &provider,
            Some(("independent work".into(), Voice::Human)),
            None,
            &bus.emitter(other.id()),
        ),
        Ok(Outcome::Complete)
    ));
    other
        .check_available()
        .expect("the independent writer remains usable");
    assert!(format!("{:?}", other.rendered_messages()).contains("still usable"));
    drop(other);
    assert!(fleet.is_empty());
}

#[test]
fn resting_helper_delivers_late_write_failure_and_poison_before_death() {
    use crate::agent::Trunk;
    use crate::agent::outcome::Outcome;
    use crate::agent::testkit::{ral_call, scripted, set_provider, trunk};
    use crate::bus::{FleetBus, Input, Next};
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{ChildStatus, Display, Voice};
    use ral_core::process::DurableRoot;
    use ral_core::test_helper::eventually;

    for poison in [false, true] {
        let mut parent = trunk(Trunk::Converses);
        let fleet = parent.fleet.clone();
        let child = parent.fork_named("failing").unwrap();
        let id = child.id();
        let recorder = child.recorder();
        let handle = child.handle();
        let path = child.log_dir().path().join("record.jsonl");
        set_provider(
            &child,
            scripted(
                "test-model",
                Script::new().then(Reply::tool_calls(vec![ral_call(
                    "reply",
                    "exarch-agents `reply ()",
                )])),
            ),
        );
        let bus = FleetBus::session();
        let parent_born = Born {
            log_dir: parent.log_dir().clone(),
            name: parent.handle().0.name.clone(),
            parent: None,
            resumed: None,
            recorder: parent.recorder(),
        };
        let mut app = App::new(
            parent.id(),
            &parent_born,
            fleet.clone(),
            bus.meter(),
            prompt(false),
            Card(Vec::new()),
            mpsc::channel().0,
        );
        let born = Born {
            log_dir: child.log_dir().clone(),
            name: "failing".into(),
            parent: Some(parent.id()),
            resumed: None,
            recorder: recorder.clone(),
        };
        let thread = child
            .launch(born, Some("reply then rest".into()), &bus.emitter(id))
            .unwrap();
        assert!(
            eventually(Duration::from_secs(5), || {
                let status = handle.status();
                (status.job.is_none() && matches!(status.report, Some(ChildStatus::Replied(_))))
                    .then_some(())
            })
            .is_some(),
            "the helper replies and rests before its writer fails"
        );
        recorder.check_available().unwrap();
        if poison {
            recorder.poison_append();
        } else {
            recorder.fail_after(11);
            assert!(
                recorder
                    .emit(Display::Answer {
                        text: "late".into()
                    })
                    .is_err()
            );
        }
        let cause = recorder.check_available().unwrap_err().to_string();
        let bytes = std::fs::read(&path).unwrap();
        let rx = bus.rx();
        let mut faults = 0;
        let mut fault = None;
        loop {
            let signal = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the failed owner retires");
            let died =
                matches!(&signal, Signal::Transient(agent, Transient::Died { .. }) if *agent == id);
            match &signal {
                Signal::Born(agent, _) => {
                    let agent = *agent;
                    apply_signal(&mut app, rx, signal);
                    app.tabs.set_focus(agent);
                    continue;
                }
                Signal::Transient(agent, Transient::Fault { text }) if *agent == id => {
                    faults += 1;
                    assert!(text.contains(&cause), "{text}");
                    fault = Some(text.clone());
                }
                _ => {}
            }
            if died {
                assert_fault_visible(&mut app, id, fault.as_deref().expect("fault precedes Died"));
                assert!(!handle.is_live());
                assert!(
                    matches!(parent.mailbox.next(REST), Next::Take {
                    input: Input::Child { id: child_id, status: ChildStatus::Replied(_), .. }, ..
                } if child_id == id),
                    "the already-owed reply precedes retirement"
                );
            }
            apply_signal(&mut app, rx, signal);
            if died {
                break;
            }
        }
        thread.join().unwrap();
        assert_eq!(faults, 1);
        assert!(app.tabs.lingering(id));
        assert_eq!(app.tabs.focused(), parent.id());
        let frozen = text(&mut app, id);
        app.transient(id, Transient::State(AgentState::Ready), rx);
        app.transient(id, Transient::Token("must not revive".into()), rx);
        assert!(matches!(
            app.tabs.status(id).unwrap().state,
            StateSpan::Ended
        ));
        assert_eq!(text(&mut app, id), frozen);
        assert_eq!(
            recorder
                .emit(Display::Answer {
                    text: "refused".into()
                })
                .unwrap_err()
                .to_string(),
            cause
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(app.is_steerable() && parent.handle().is_live());
        let provider = scripted(
            "test-model",
            Script::new().then(Reply::text("still usable")),
        );
        assert!(matches!(
            parent.deliberate(
                &DurableRoot::default().child(),
                &provider,
                Some(("independent work".into(), Voice::Human)),
                None,
                &bus.emitter(parent.id()),
            ),
            Ok(Outcome::Complete)
        ));
        assert!(parent.check_available().is_ok());
        assert!(format!("{:?}", parent.rendered_messages()).contains("still usable"));
        drop(parent);
        assert!(fleet.is_empty());
    }
}

#[test]
fn root_completion_not_directory_emptiness_certifies_the_tui_final_drain() {
    use crate::agent::Trunk;
    use crate::agent::nursery::Joined;
    use crate::agent::spawn::hatch_root;
    use crate::agent::testkit::{host, trunk};
    use crate::bus::FleetBus;
    use crate::record::Memory;
    use ral_core::test_helper::eventually;

    for poison in [false, true] {
        let control = trunk(Trunk::Converses);
        let root = hatch_root(&control, Memory::Amnemon, Some("last".into())).unwrap();
        let host = host(control);
        host.roots()[0].end();
        let fleet = host.fleet().clone();
        let id = root.id();
        let handle = root.handle();
        let recorder = root.recorder();
        let bus = FleetBus::session();
        let born = Born {
            log_dir: root.log_dir().clone(),
            name: "last".into(),
            parent: None,
            resumed: None,
            recorder: recorder.clone(),
        };
        let emit = bus.emitter(id).child(id, born.clone());
        let mut app = App::new(
            id,
            &born,
            fleet.clone(),
            bus.meter(),
            prompt(false),
            Card(Vec::new()),
            mpsc::channel().0,
        );
        // Pause the root producer at the name-removal / terminal-publication seam.
        let (arrived, before_terminal) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        handle
            .steer("recording fails before provider work".into())
            .unwrap();
        if poison {
            recorder.poison_append();
        } else {
            recorder.fail_after(7);
        }
        let terminal_recorder = recorder.clone();
        fleet.adopt_root(Joined::new(std::thread::spawn(move || {
            let mut teardown = root.run(&emit);
            let failure = terminal_recorder.check_available().unwrap_err();
            crate::agent::spawn::announce(&failure, &emit);
            teardown.failures.push(failure.to_string());
            arrived.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            emit.fleet_sink().transient(Transient::Died {
                failures: teardown.failures.clone(),
            });
            teardown
        })));
        before_terminal
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(eventually(Duration::from_secs(5), || fleet.is_empty().then_some(())).is_some());
        assert!(!handle.is_live());
        assert!(
            !host.is_finished(),
            "an empty directory does not certify the final cue"
        );
        let rx = bus.rx();
        let mut signals = Vec::new();
        while let Ok(signal) = rx.try_recv() {
            assert!(!matches!(
                &signal,
                Signal::Transient(_, Transient::Died { .. })
            ));
            signals.push(signal);
        }
        release.send(()).unwrap();
        assert!(eventually(Duration::from_secs(5), || host.is_finished().then_some(())).is_some());
        while let Ok(signal) = rx.try_recv() {
            signals.push(signal);
        }
        let mut deaths = 0;
        let mut faults = 0;
        let mut fault = None;
        for signal in signals {
            if matches!(&signal, Signal::Born(..)) {
                continue;
            }
            match &signal {
                Signal::Transient(_, Transient::Fault { text }) => {
                    faults += 1;
                    fault = Some(text.clone());
                }
                Signal::Transient(_, Transient::Died { .. }) => {
                    deaths += 1;
                    assert_fault_visible(
                        &mut app,
                        id,
                        fault.as_deref().expect("fault precedes Died"),
                    );
                }
                _ => {}
            }
            apply_signal(&mut app, rx, signal);
        }
        assert_eq!(deaths, 1);
        assert_eq!(faults, 1);
        assert!(recorder.check_available().is_err());
        assert!(matches!(
            app.tabs.status(id).unwrap().state,
            StateSpan::Ended
        ));
        assert!(app.tabs.lingering(id));
        assert!(!app.is_steerable());
        assert_fault_visible(&mut app, id, fault.as_deref().unwrap());
        assert_eq!(
            host.end(),
            [recorder.check_available().unwrap_err().to_string()]
        );
    }
}

#[test]
fn live_vi_completion_escape_undo_and_redo_restore_the_partial_command() {
    let (mut app, _, _root) = app();
    app.prompt_state = prompt(true);
    app.prompt_state.set_prompt("/th");
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.key(key(KeyCode::Char('i')), &ctx());
    app.key(key(KeyCode::Enter), &ctx());
    assert_eq!(app.prompt_state.prompt_text(), "/thinking");
    app.key(key(KeyCode::Esc), &ctx());
    assert!(app.prompt_state.menu().is_none());
    app.key(key(KeyCode::Char('u')), &ctx());
    assert_eq!(app.prompt_state.prompt_text(), "/thi");
    app.key(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        &ctx(),
    );
    assert_eq!(app.prompt_state.prompt_text(), "/thinking");
}

#[test]
fn escape_and_ctrl_c_stop_the_recorded_root_or_helper_job_and_recall_text() {
    use crate::bus::{Command, Input, Next, Read};
    use crate::record::Forensic;
    use ral_core::process::CancelCause;

    for helper_focused in [false, true] {
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let fleet = Fleet::for_test();
            let root = test_agent(&fleet, TestAgentSpec::new("main")).unwrap();
            let mut app = App::new(
                root.id(),
                &Born::for_test("main", None),
                fleet.clone(),
                UsageMeter::default(),
                prompt(false),
                Card(Vec::new()),
                mpsc::channel().0,
            );
            let child = test_agent(
                &fleet,
                TestAgentSpec::child("child", &root, root.mailbox.life().child()),
            )
            .unwrap();
            born(&mut app, child.id(), "child", root.id());
            let root_exchange = open_exchange(&mut app, &root);
            let child_exchange = open_exchange(&mut app, &child);
            let (focused, exchange, untouched) = if helper_focused {
                (&child, &child_exchange, &root_exchange)
            } else {
                (&root, &root_exchange, &child_exchange)
            };
            app.tabs.set_focus(focused.id());
            let job = app.tabs.focused_job().expect("JobOpened addresses Stop");
            focused.steer("one".into()).unwrap();
            focused.post_command(Command::Read(Read::Resources));
            focused.steer("two\n界\n".into()).unwrap();
            app.prompt_state.set_prompt("unfinished draft");

            app.key(key, &ctx());

            assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
            assert!(!untouched.is_cancelled());
            assert!(root.is_live() && child.is_live(), "Stop ends no lives");
            assert_eq!(focused.queued(), ["/resources"]);
            assert_eq!(
                app.prompt_state.prompt_text(),
                "one\n\ntwo\n界\n\n\nunfinished draft"
            );
            assert_eq!(app.tabs.focused_job(), Some(job), "Stop is not settlement");
            assert!(matches!(
                focused.mailbox.next(REST),
                Next::Command(Command::Read(Read::Resources))
            ));
            assert!(matches!(
                focused.mailbox.next(REST),
                Next::Settled { stopped: true }
            ));
            job_commit(
                &mut app,
                focused.id(),
                job.get() + 1,
                Forensic::JobSettled { job, stopped: true },
            );
            assert_eq!(app.tabs.focused_job(), None);
            app.key(key, &ctx());
            assert_eq!(
                app.prompt_state.prompt_text(),
                "one\n\ntwo\n界\n\n\nunfinished draft"
            );

            focused.steer("next".into()).unwrap();
            let Next::Take {
                input: Input::Human(text),
                exchange: next,
            } = focused.mailbox.next(REST)
            else {
                panic!("a new exchange takes the next prompt");
            };
            assert_eq!(text, "next");
            assert!(!next.is_cancelled());
            assert!(child.is_live());
        }
    }
}

#[test]
fn a_root_parked_on_a_working_helper_reads_as_waiting_on_agents() {
    let (mut app, rx, root, fleet) = app_in_fleet(&Born::for_test("main", None));
    let child = test_agent(
        &fleet,
        TestAgentSpec::child("child", &root, root.mailbox.life().child()),
    )
    .unwrap();
    born(&mut app, child.id(), "child", root.id());
    app.transient(
        root.id(),
        Transient::State(AgentState::WaitingOnAgents),
        &rx,
    );

    let state = app.tabs.status(root.id()).unwrap().state;
    assert!(matches!(
        state,
        StateSpan::Active {
            state: AgentState::WaitingOnAgents,
            ..
        }
    ));
    assert!(state.pending(), "a wait on the fleet is outstanding work");
    let rule: String =
        crate::tui::status::rule_line(80, state, None, &Usage::default(), 0, None, "m")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
    assert!(rule.contains("waiting on agents"), "{rule}");
}

#[test]
fn escape_on_a_root_with_a_working_helper_keeps_the_helper_and_its_report_continues_the_job() {
    use crate::bus::{Input, Next};
    use crate::record::{ChildStatus, Forensic};
    use ral_core::process::CancelCause;

    let (mut app, _rx, root, fleet) = app_in_fleet(&Born::for_test("main", None));
    let helper = test_agent(
        &fleet,
        TestAgentSpec::child("helper", &root, root.mailbox.life().child()),
    )
    .unwrap();
    born(&mut app, helper.id(), "helper", root.id());
    let root_exchange = open_exchange(&mut app, &root);
    let job = root.status().job.expect("the root's job is open");

    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctx());

    assert_eq!(root_exchange.cause(), Some(CancelCause::Interrupted));
    assert!(helper.is_live(), "Stop preserves helpers");
    assert!(!helper.mailbox.life().is_cancelled());
    helper.report(ChildStatus::Stopped);
    let Next::Take {
        input: Input::Child { id, .. },
        exchange,
    } = root.mailbox.next(REST)
    else {
        panic!("the helper's report wakes the stopped root");
    };
    assert_eq!(id, helper.id());
    assert!(!exchange.is_cancelled(), "the report runs a live exchange");
    assert!(root.mailbox.begin());
    assert_eq!(root.status().job, Some(job), "the same job continues");
    assert!(matches!(
        root.mailbox.next(REST),
        Next::Settled { stopped: true }
    ));
    job_commit(
        &mut app,
        root.id(),
        job.get() + 1,
        Forensic::JobSettled { job, stopped: true },
    );
    assert_eq!(app.tabs.focused_job(), None);
}

#[test]
fn escape_waits_for_job_opened_and_does_nothing_after_job_settled() {
    use crate::bus::Next;
    use crate::record::Forensic;

    let (mut app, _, root) = app();
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    root.steer("waiting at rest".into()).unwrap();
    app.prompt_state.set_prompt("draft");
    app.key(esc, &ctx());
    assert_eq!(root.queued(), ["waiting at rest"]);
    let Next::Take { exchange, .. } = root.mailbox.next(REST) else {
        panic!("queued input");
    };
    assert!(root.mailbox.begin());
    let job = root.status().job.unwrap();
    app.key(esc, &ctx());
    assert!(
        !exchange.is_cancelled(),
        "the UI has not observed JobOpened"
    );
    job_commit(&mut app, root.id(), job.get(), Forensic::JobOpened);
    app.key(esc, &ctx());
    assert!(exchange.is_cancelled());
    assert!(matches!(
        root.mailbox.next(REST),
        Next::Settled { stopped: true }
    ));
    job_commit(
        &mut app,
        root.id(),
        job.get() + 1,
        Forensic::JobSettled { job, stopped: true },
    );
    root.steer("future prompt".into()).unwrap();
    app.key(esc, &ctx());
    assert_eq!(root.queued(), ["future prompt"]);
    assert_eq!(app.prompt_state.prompt_text(), "draft");
}

#[test]
fn popup_and_overlay_own_escape_before_stop() {
    use crate::record::{NonEmpty, Protocol, Record, Voice};

    let (mut app, _, root) = app();
    let exchange = open_exchange(&mut app, &root);
    root.steer("queued".into()).unwrap();
    app.prompt_state.set_prompt("/th");
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctx());
    assert!(app.prompt_state.menu().is_none());
    assert!(!exchange.is_cancelled());
    assert_eq!(root.queued(), ["queued"]);
    assert_eq!(app.prompt_state.prompt_text(), "/th");

    app.commit(
        root.id(),
        &Commit::placeholder(
            2,
            NonEmpty::one(Record::Protocol(Protocol::UserPrompt {
                turn: 1,
                text: "hello".into(),
                voice: Voice::Human,
            })),
        ),
    );
    for cancel in [
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        rewind::rewind(&mut app);
        assert!(app.overlay.is_some());
        app.key(cancel, &ctx());
        assert!(app.overlay.is_none());
        assert!(!exchange.is_cancelled());
        assert_eq!(root.queued(), ["queued"]);
        assert_eq!(app.prompt_state.prompt_text(), "/th");
    }
}

#[test]
fn unbracketed_burst_controls_reach_the_draft_without_accepting_completion() {
    use crate::tui::paste_burst::{Input, PasteBurst};
    use std::time::Instant;

    let (mut app, _, _root) = app();
    app.prompt_state.set_prompt("/t");
    assert!(app.prompt_state.menu().is_some());
    let now = Instant::now();
    let mut burst = PasteBurst::default();
    for code in [KeyCode::Char('h'), KeyCode::Enter, KeyCode::Tab] {
        assert_eq!(burst.key(KeyEvent::new(code, KeyModifiers::NONE), now), []);
    }
    for input in burst.flush_due(burst.deadline().expect("pending burst")) {
        match input {
            Input::Key(key) => assert!(!app.key(key, &ctx())),
            Input::Paste(text) => app.paste(&text, &ctx()),
        }
    }
    assert_eq!(app.prompt_state.prompt_text(), "/th\n\t");
    assert!(app.prompt_state.menu().is_none());
}

#[test]
fn watch_only_tabs_cannot_paste_or_accept_a_popup() {
    let (mut app, _, root) = app();
    app.prompt_state.set_prompt("/th");
    let child = AgentId::new(root.id().get() + 1);
    born(&mut app, child, "watch", root.id());
    app.tabs.set_focus(child);
    assert!(!app.is_steerable());
    app.paste("must not enter the draft", &ctx());
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx());
    assert_eq!(app.prompt_state.prompt_text(), "/th");
}

#[test]
fn modified_enter_neither_submits_nor_accepts_completion() {
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
    ] {
        let (mut app, _, _root) = app();
        app.prompt_state.set_prompt("/th");
        app.key(KeyEvent::new(KeyCode::Enter, modifiers), &ctx());
        let draft = app.prompt_state.prompt_text();
        assert_ne!(draft, "");
        assert_ne!(draft, "/thinking");
        assert_eq!(
            key_action(&KeyEvent::new(KeyCode::Enter, modifiers), true),
            KeyAction::Edit
        );
    }
}

#[test]
fn routing_boundaries_disarm_the_external_editor_prefix() {
    for code in [
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Char('c'),
        KeyCode::Tab,
    ] {
        let (mut app, _, root) = app();
        born(
            &mut app,
            AgentId::new(root.id().get() + 1),
            "child",
            root.id(),
        );
        app.prefix = Some(Prefix::CtrlX);
        let modifiers = if code == KeyCode::Char('c') {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        assert!(!app.key(KeyEvent::new(code, modifiers), &ctx()));
        assert_eq!(app.prefix, None, "{code:?} must disarm C-x");
    }
}

#[test]
fn external_editor_chords_return_to_the_loop_without_submitting() {
    let (mut app, _, root) = app();
    let exchange = open_exchange(&mut app, &root);
    app.prompt_state.set_prompt("draft");
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    assert!(app.key(ctrl('g'), &ctx()));
    assert!(!app.key(ctrl('x'), &ctx()));
    assert!(app.key(ctrl('e'), &ctx()));
    assert_eq!(app.prefix, None);
    assert!(!exchange.is_cancelled());
    assert_eq!(app.prompt_state.prompt_text(), "draft");
}

fn copy_chords() -> Vec<KeyEvent> {
    let mut chords = Vec::new();
    for c in ['c', 'C'] {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::SUPER,
        ] {
            chords.push(KeyEvent::new(KeyCode::Char(c), modifiers));
        }
    }
    chords.push(KeyEvent::new(KeyCode::Insert, KeyModifiers::CONTROL));
    chords
}

#[test]
fn copy_chords_recognize_normalized_aliases_only() {
    for chord in copy_chords() {
        assert!(copy_key(&chord), "{chord:?}");
    }
    for chord in [
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT),
        KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ),
        KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT),
    ] {
        assert!(!copy_key(&chord), "{chord:?}");
    }
}

fn select_prompt(app: &mut App) -> String {
    app.prompt_state.set_prompt("select this draft");
    mouse_frame(app);
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), 2),
        (MouseEventKind::Drag(MouseButton::Left), 8),
        (MouseEventKind::Up(MouseButton::Left), 8),
    ] {
        left_mouse(app, kind, column, 10);
    }
    app.prompt_state.selection_text().expect("selected prompt")
}

#[test]
fn every_copy_alias_preserves_failed_selection_and_retries_without_interrupting() {
    for chord in copy_chords() {
        let (mut app, _, root) = app();
        let exchange = open_exchange(&mut app, &root);
        let selected = select_prompt(&mut app);
        let expected = selected.clone();
        let attempts = std::rc::Rc::new(std::cell::Cell::new(0));
        let writes = attempts.clone();
        app.copy_sink = Some(Box::new(move |text| {
            assert_eq!(text, expected);
            let attempt = writes.get();
            writes.set(attempt + 1);
            if attempt == 0 {
                Err(std::io::Error::other("scripted clipboard failure"))
            } else {
                Ok(clipboard::Copied::Whole)
            }
        }));
        app.key(chord, &ctx());
        assert_eq!(attempts.get(), 1);
        assert_eq!(app.prompt_state.selection_text(), Some(selected));
        assert!(!exchange.is_cancelled());
        let id = app.tabs.focused();
        assert!(text(&mut app, id).contains("scripted clipboard failure"));
        app.key(chord, &ctx());
        assert_eq!(attempts.get(), 2);
        assert!(app.prompt_state.selection_text().is_none());
        assert!(!exchange.is_cancelled());
        assert_eq!(app.prompt_state.prompt_text(), "select this draft");
    }
}

#[test]
fn copy_chord_is_gated_like_right_click_copy() {
    let (mut app, _, root) = app();
    let exchange = open_exchange(&mut app, &root);
    let selected = select_prompt(&mut app);
    app.copy_sink = Some(Box::new(|_| panic!("copy is gated")));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    let child = AgentId::new(root.id().get() + 1);
    born(&mut app, child, "watch", root.id());
    app.tabs.set_focus(child);
    assert!(!app.is_steerable());
    app.key(ctrl_c, &ctx());
    app.tabs.set_focus(root.id());
    assert_eq!(app.prompt_state.selection_text(), Some(selected.clone()));

    app.matrix_nav(Nav::Toggle);
    assert!(app.matrix_navigating());
    app.key(ctrl_c, &ctx());
    assert!(app.matrix_navigating());
    assert_eq!(app.prompt_state.selection_text(), Some(selected));
    assert!(
        exchange.is_cancelled(),
        "Ctrl-C interrupts under the matrix"
    );
}

#[test]
fn right_click_copies_only_an_explicit_prompt_selection() {
    let (mut app, _, root) = app();
    let exchange = open_exchange(&mut app, &root);
    let selected = select_prompt(&mut app);
    app.copy_sink = Some(Box::new(|_| panic!("right click outside prompt")));
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Right), 2, 1);
    assert_eq!(app.prompt_state.selection_text(), Some(selected.clone()));
    app.copy_sink = Some(Box::new(|_| Err(std::io::Error::other("try again"))));
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Right), 4, 10);
    assert_eq!(app.prompt_state.selection_text(), Some(selected.clone()));
    app.copy_sink = Some(Box::new(move |text| {
        assert_eq!(text, selected);
        Ok(clipboard::Copied::Whole)
    }));
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Right), 4, 10);
    assert!(app.prompt_state.selection_text().is_none());
    app.copy_sink = Some(Box::new(|_| panic!("no selection to copy")));
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Right), 4, 10);
    assert!(app.mouse_owner.is_none());
    assert!(!exchange.is_cancelled());
}

#[test]
fn right_click_does_not_steal_an_owned_drag() {
    let (mut app, _, _root) = app();
    select_prompt(&mut app);
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 10);
    left_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 8, 10);
    let selected = app.prompt_state.selection_text();
    assert!(selected.is_some());
    app.copy_sink = Some(Box::new(|_| panic!("a left drag owns the gesture")));
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Right), 4, 10);
    assert!(matches!(app.mouse_owner, Some(MouseOwner::Prompt(_))));
    assert_eq!(app.prompt_state.selection_text(), selected);
    left_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 8, 10);
    assert!(app.mouse_owner.is_none());
}

#[test]
fn prompt_mouse_selection_does_not_start_transcript_selection() {
    let (mut app, _, root) = app();
    let exchange = open_exchange(&mut app, &root);
    app.prompt_state.set_prompt("select this draft");
    mouse_frame(&mut app);
    let select = |app: &mut App| {
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 2),
            (MouseEventKind::Drag(MouseButton::Left), 8),
            (MouseEventKind::Up(MouseButton::Left), 8),
        ] {
            left_mouse(app, kind, column, 10);
        }
    };
    select(&mut app);
    assert!(app.gesture.selection().is_none());
    assert!(app.prompt_state.selection_text().is_some());
    app.key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &ctx(),
    );
    assert!(
        !exchange.is_cancelled(),
        "copy must not interrupt the agent"
    );
    assert_eq!(app.prompt_state.prompt_text(), "select this draft");
}

#[test]
fn history_search_enter_accepts_without_submitting() {
    let (mut app, _, _root) = app();
    app.prompt_state.set_prompt("historical prompt");
    assert!(app.prompt_state.submit().is_some());
    app.prompt_state.set_prompt("draft");
    app.key(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        &ctx(),
    );
    assert!(app.prompt_state.is_searching());
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx());
    assert!(!app.prompt_state.is_searching());
    assert_eq!(app.prompt_state.prompt_text(), "historical prompt");
}

#[test]
fn history_search_cancel_preserves_the_draft_without_interrupting() {
    for cancel in [
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let (mut app, _, root) = app();
        let exchange = open_exchange(&mut app, &root);
        app.prompt_state.set_prompt("historical prompt");
        assert!(app.prompt_state.submit().is_some());
        app.prompt_state.set_prompt("unfinished draft");
        app.key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &ctx(),
        );
        assert!(app.prompt_state.is_searching());
        assert!(!app.key(cancel, &ctx()));
        assert!(!app.prompt_state.is_searching());
        assert_eq!(app.prompt_state.prompt_text(), "unfinished draft");
        assert!(!exchange.is_cancelled());
    }
}

#[test]
fn matrix_cursor_does_not_change_focus_until_enter() {
    let (mut app, _, root) = app();
    let child = AgentId::new(root.id().get() + 1);
    born(&mut app, child, "child", root.id());

    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.key(key(KeyCode::Tab), &ctx());
    assert!(app.matrix_navigating(), "Tab enters the matrix");
    assert_eq!(
        app.matrix_cursor(&app.tabs.rows()),
        Some(root.id()),
        "the cursor starts on the attached agent"
    );

    app.key(key(KeyCode::Down), &ctx());
    assert_eq!(
        app.matrix_cursor(&app.tabs.rows()),
        Some(child),
        "arrows move the matrix cursor"
    );
    assert_eq!(
        app.tabs.focused(),
        root.id(),
        "moving the cursor does not retarget the prompt"
    );

    app.key(key(KeyCode::Enter), &ctx());
    assert_eq!(app.tabs.focused(), child, "Enter attaches to the cursor");
    assert!(!app.matrix_navigating(), "and leaves the matrix");
}

#[test]
fn esc_leaves_the_matrix_without_moving_focus_or_stopping_the_job() {
    let (mut app, _, root) = app();
    let exchange = open_exchange(&mut app, &root);
    root.steer("queued".into()).unwrap();
    born(
        &mut app,
        AgentId::new(root.id().get() + 1),
        "child",
        root.id(),
    );

    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.key(key(KeyCode::Tab), &ctx());
    app.key(key(KeyCode::Down), &ctx());
    app.key(key(KeyCode::Esc), &ctx());

    assert!(!app.matrix_navigating(), "Esc leaves the surface");
    assert!(!exchange.is_cancelled());
    assert_eq!(root.queued(), ["queued"]);
    assert_eq!(
        app.tabs.focused(),
        root.id(),
        "an abandoned cursor attaches to nothing"
    );
}

#[test]
fn matrix_mode_swallows_prompt_editing() {
    let (mut app, _, root) = app();
    born(
        &mut app,
        AgentId::new(root.id().get() + 1),
        "child",
        root.id(),
    );
    app.prompt_state.set_prompt("draft");
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &ctx());
    app.key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &ctx(),
    );
    assert_eq!(
        app.prompt_state.prompt_text(),
        "draft",
        "matrix keys cannot edit the prompt"
    );
}

/// A pin is ambient register state: it lands in the scrollback's own
/// register, never in the mirror, so it cannot split the run it is offered
/// to no block of.  The call and its two reads are one group either side
/// of it.
#[test]
fn a_pin_never_splits_a_coalesced_observation_run() {
    use crate::record::{Commit, Display, NonEmpty, Record};
    use ral_core::data::datum::Datum as _;
    use ral_core::fact::Read;

    let (mut app, rx, root) = app();
    let opening = app
        .tabs
        .scrollback(root.id())
        .expect("root has a scrollback")
        .probe_figures()
        .0;
    let mut seq = 0;
    let mut fact = |app: &mut App, display| {
        seq += 1;
        app.commit(
            root.id(),
            &Commit::placeholder(seq, NonEmpty::one(Record::Display(display))),
        );
    };
    let read_at = |path: &str| {
        Observation::instant(None, None, Observed::Read(Read { path: path.into() })).encode()
    };

    fact(
        &mut app,
        Display::ToolCall {
            tool: "ral".into(),
            cmd: "read 'a.rs'".into(),
            summary: Some("look around".into()),
        },
    );
    fact(
        &mut app,
        Display::Observation {
            value: read_at("a.rs"),
        },
    );
    app.transient(
        root.id(),
        Transient::Pin {
            key: "tasks".into(),
            card: Card(vec![Mark::Raw {
                bytes: b"one left".to_vec(),
            }]),
        },
        &rx,
    );
    fact(
        &mut app,
        Display::Observation {
            value: read_at("b.rs"),
        },
    );

    let sb = app
        .tabs
        .scrollback(root.id())
        .expect("root has a scrollback");
    assert_eq!(
        sb.probe_figures().0 - opening,
        1,
        "the call and the two reads it produced are one block"
    );
    assert_eq!(
        app.tabs
            .status(root.id())
            .expect("root has a status")
            .pins()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["tasks"],
        "the pin lands in the register, never in scrollback"
    );
    let all = text(&mut app, root.id());
    assert!(
        all.contains("a.rs") && all.contains("b.rs"),
        "both reads render in the one block: {all:?}"
    );
}

/// A tab in the linger window has rendered its final frame: a straggler
/// from a worker whose cancel it outran must not paint into it.
#[test]
fn a_dying_tab_admits_no_straggler() {
    use crate::record::{Commit, Display, NonEmpty, Record};

    let (mut app, rx, root) = app();
    let helper = AgentId::new(root.id().get() + 1);
    born(&mut app, helper, "helper", root.id());
    let number = 1;
    app.commit(
        helper,
        &Commit::placeholder(
            number,
            NonEmpty::one(Record::Display(Display::Answer {
                text: "alive".into(),
            })),
        ),
    );
    app.transient(helper, Transient::Died { failures: vec![] }, &rx);
    let blocks = app
        .tabs
        .scrollback(helper)
        .expect("the child keeps its scrollback through the linger window")
        .probe_figures()
        .0;

    let number = 2;
    app.commit(
        helper,
        &Commit::placeholder(
            number,
            NonEmpty::one(Record::Display(Display::Answer {
                text: "straggler".into(),
            })),
        ),
    );

    let all = text(&mut app, helper);
    assert!(all.contains("alive"), "the final frame survives: {all:?}");
    assert!(
        !all.contains("straggler"),
        "a dying tab admits no post-mortem text: {all:?}"
    );
    assert_eq!(
        app.tabs
            .scrollback(helper)
            .expect("scrollback")
            .probe_figures()
            .0,
        blocks,
        "and gains no block from the events it dropped"
    );
}

/// A `/clear` starts the mirror over, and the agent's pins and state with it.
#[test]
fn a_clear_wipes_the_pins_and_the_state() {
    use crate::bus::Signal;
    use crate::record::{Emitter as Recorder, Protocol, Voice};

    let dir = ral_core::own::OwnDir::for_test("clear").expect("a test directory");
    let recorder = Recorder::create(&dir).expect("a record log");
    let (tx, rx) = crate::bus::channel();
    recorder.attach(crate::bus::Emitter::new(tx, AgentId::new(0)).fleet_sink());
    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh root");
    let born = Born {
        log_dir: ral_core::own::OwnDir::clone(&dir),
        recorder: recorder.clone(),
        ..Born::for_test("main", None)
    };
    let mut app = App::new(
        root.id(),
        &born,
        fleet,
        UsageMeter::default(),
        prompt(false),
        Card(Vec::new()),
        mpsc::channel().0,
    );
    let hear = |app: &mut App| {
        while let Ok(Signal::Commit(_, rec)) = rx.try_recv() {
            app.commit(root.id(), &rec);
        }
    };

    let _recorded = recorder
        .emit(Protocol::UserPrompt {
            turn: 1,
            text: "before".into(),
            voice: Voice::Human,
        })
        .expect("the log appends");
    hear(&mut app);
    app.transient(root.id(), Transient::State(AgentState::Evaluating), &rx);
    app.transient(
        root.id(),
        Transient::Pin {
            key: "tasks".into(),
            card: Card(Vec::new()),
        },
        &rx,
    );
    let status = app.tabs.status(root.id()).expect("root has a status");
    assert!(
        matches!(
            status.state,
            StateSpan::Active {
                state: AgentState::Evaluating,
                ..
            }
        ) && !status.pins().is_empty()
    );

    let _recorded = recorder.emit(Protocol::Cleared).expect("the log appends");
    hear(&mut app);

    let status = app.tabs.status(root.id()).expect("root has a status");
    assert!(
        matches!(
            status.state,
            StateSpan::Active {
                state: AgentState::Ready,
                ..
            }
        ) && status.pins().is_empty(),
        "the clear wiped both"
    );
}

/// Enter in `/rewind` rewinds the tab the overlay was opened on, whichever
/// tab has focus by then.
#[test]
fn rewind_acts_on_the_tab_it_was_opened_on() {
    use crate::bus::{Command, Next, Rewrite};
    use crate::record::{Commit, NonEmpty, Protocol, Record, Voice};

    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh root");
    let mut app = App::new(
        root.id(),
        &Born::for_test("main", None),
        fleet,
        UsageMeter::default(),
        prompt(false),
        Card(Vec::new()),
        mpsc::channel().0,
    );
    let child = AgentId::new(root.id().get() + 1);
    born(&mut app, child, "child", root.id());
    app.commit(
        root.id(),
        &Commit::placeholder(
            1,
            NonEmpty::one(Record::Protocol(Protocol::UserPrompt {
                turn: 1,
                text: "hello".into(),
                voice: Voice::Human,
            })),
        ),
    );

    rewind::rewind(&mut app);
    app.tabs.set_focus(child);
    app.overlay_input(&Input::Key(KeyCode::Enter), &ctx());

    assert!(app.overlay.is_none(), "Enter closes the overlay");
    assert!(
        matches!(
            root.mailbox.next(REST),
            Next::Command(Command::Rewrite(Rewrite::Rewind(1)))
        ),
        "the rewind went to the tab it was opened on"
    );
}

/// A frame of another tab drops the selection made on the last one.
#[test]
fn a_frame_of_another_tab_clears_the_selection() {
    use crate::tui::gesture::{FrameGeom, GestureState};
    use ratatui::layout::Rect;

    let at = |kind, column| MouseEvent {
        kind,
        column,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    let frame = |tab| FrameGeom {
        text: Rect::new(0, 0, 20, 5),
        offset: 0,
        tab,
    };
    let (a, b) = (AgentId::new(1), AgentId::new(2));
    let mut gesture = GestureState::new();
    gesture.record_frame(frame(a));
    gesture.press(at(MouseEventKind::Down(MouseButton::Left), 1));
    let _ = gesture.drag(at(MouseEventKind::Drag(MouseButton::Left), 4), 3);
    assert!(gesture.selection().is_some());

    gesture.record_frame(frame(a));
    assert!(gesture.selection().is_some(), "the same tab keeps it");
    gesture.record_frame(frame(b));
    assert!(gesture.selection().is_none(), "another tab drops it");
    assert!(gesture.drawn_for(b) && !gesture.drawn_for(a));
}

fn mouse_frame(app: &mut App) {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("test terminal");
    terminal
        .draw(|frame| app.prompt_state.render(frame, Rect::new(2, 10, 30, 4)))
        .expect("prompt frame");
    app.gesture.record_frame(super::super::gesture::FrameGeom {
        text: Rect::new(0, 0, 40, 8),
        offset: 0,
        tab: app.tabs.focused(),
    });
}

fn left_mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
    app.mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn transcript_drag_keeps_release_when_it_enters_the_prompt() {
    let (mut app, _, _root) = app();
    app.prompt_state.set_prompt("untouched draft");
    mouse_frame(&mut app);
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 1);
    left_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 8, 10);
    assert_eq!(app.mouse_owner, Some(MouseOwner::Transcript));
    assert!(app.gesture.selection().is_some());
    left_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 8, 10);
    assert_eq!(app.mouse_owner, None);
    assert!(app.gesture.selection().is_some());
    assert_eq!(app.prompt_state.prompt_text(), "untouched draft");
    assert!(app.prompt_state.selection_text().is_none());
}

#[test]
fn prompt_drag_owns_motion_and_release_outside_the_prompt() {
    let (mut app, _, _root) = app();
    app.prompt_state.set_prompt("select this draft");
    mouse_frame(&mut app);
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 10);
    left_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 8, 1);
    assert!(matches!(app.mouse_owner, Some(MouseOwner::Prompt(_))));
    assert!(app.gesture.selection().is_none());
    left_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 8, 1);
    assert_eq!(app.mouse_owner, None);
    left_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 9, 10);
    assert!(app.gesture.selection().is_none());
}

#[test]
fn shift_gesture_stays_terminal_owned_when_shift_is_released_first() {
    let (mut app, _, _root) = app();
    app.prompt_state.set_prompt("untouched draft");
    mouse_frame(&mut app);
    app.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 2,
        row: 10,
        modifiers: KeyModifiers::SHIFT,
    });
    left_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 8, 1);
    assert_eq!(app.mouse_owner, Some(MouseOwner::Terminal));
    left_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 8, 1);
    assert_eq!(app.mouse_owner, None);
    assert!(app.gesture.selection().is_none());
    assert!(app.prompt_state.selection_text().is_none());
}

#[test]
fn only_a_shifted_left_press_latches_terminal_ownership() {
    let (mut app, _, _root) = app();
    let id = app.tabs.focused();
    for n in 0..30 {
        app.push_error(id, &format!("row {n}"));
    }
    mouse_frame(&mut app);
    let offset = |app: &mut App| {
        app.tabs
            .scrollback_mut(id)
            .expect("the tab under test has a scrollback")
            .render_window(READ_W, 5)
            .offset
    };
    let bottom = offset(&mut app);
    for kind in [
        MouseEventKind::ScrollUp,
        MouseEventKind::Moved,
        MouseEventKind::Down(MouseButton::Right),
    ] {
        app.mouse(MouseEvent {
            kind,
            column: 2,
            row: 1,
            modifiers: KeyModifiers::SHIFT,
        });
        assert_eq!(app.mouse_owner, None, "{kind:?}");
    }
    left_mouse(&mut app, MouseEventKind::ScrollUp, 2, 1);
    assert!(offset(&mut app) < bottom, "a plain wheel still scrolls");
}

#[test]
fn focus_and_modal_changes_cancel_a_prompt_drag_and_prefix() {
    for matrix in [false, true] {
        let (mut app, _, root) = app();
        let child = AgentId::new(root.id().get() + 1);
        born(&mut app, child, "watch", root.id());
        app.prompt_state.set_prompt("draft");
        mouse_frame(&mut app);
        left_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 10);
        app.prefix = Some(Prefix::CtrlX);
        if matrix {
            app.matrix_nav(Nav::Toggle);
        } else {
            app.tabs.set_focus(child);
            app.reconcile_input_focus();
        }
        assert_eq!(app.mouse_owner, None);
        assert_eq!(app.prefix, None);
        left_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 8, 1);
        assert!(app.gesture.selection().is_none());
        assert_eq!(app.prompt_state.prompt_text(), "draft");
    }
}

#[test]
fn a_key_ends_a_prompt_drag_whose_anchor_it_may_invalidate() {
    for paste in [false, true] {
        let (mut app, _, _root) = app();
        app.prompt_state.set_prompt("select this draft");
        mouse_frame(&mut app);
        left_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 20, 10);
        assert!(matches!(app.mouse_owner, Some(MouseOwner::Prompt(_))));
        if paste {
            app.paste("x", &ctx());
        } else {
            app.key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                &ctx(),
            );
        }
        assert_eq!(app.mouse_owner, None);
        let draft = app.prompt_state.prompt_text();
        left_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 4, 10);
        left_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 4, 10);
        assert!(app.prompt_state.selection_text().is_none());
        assert!(app.gesture.selection().is_none());
        assert_eq!(app.prompt_state.prompt_text(), draft);
    }
}

#[test]
fn a_transcript_gesture_survives_keys() {
    let (mut app, _, _root) = app();
    mouse_frame(&mut app);
    left_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 1);
    app.key(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        &ctx(),
    );
    assert_eq!(app.mouse_owner, Some(MouseOwner::Transcript));
}

#[test]
fn editor_search_owns_enter_escape_and_external_editor_shortcuts() {
    for finish in [KeyCode::Enter, KeyCode::Esc] {
        let (mut app, _, root) = app();
        let exchange = open_exchange(&mut app, &root);
        app.prompt_state = prompt(true);
        app.prompt_state.set_prompt("searchable draft");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // Set up Normal mode without the application's Escape interrupt.
        app.prompt_state.edit_input(key(KeyCode::Esc));
        assert!(!exchange.is_cancelled());
        app.key(key(KeyCode::Char('/')), &ctx());
        assert!(app.prompt_state.editor_is_searching());
        for ch in "draft".chars() {
            app.key(key(KeyCode::Char(ch)), &ctx());
        }
        assert!(!app.key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            &ctx(),
        ));
        assert!(app.prompt_state.editor_is_searching());
        assert!(!app.key(key(finish), &ctx()));
        assert!(!app.prompt_state.editor_is_searching());
        assert!(!app.prompt_state.is_searching());
        assert!(!exchange.is_cancelled());
        assert_eq!(app.prompt_state.prompt_text(), "searchable draft");
    }
}

#[test]
fn stale_escape_reaches_vi_editor_but_ctrl_c_does_not() {
    use crate::record::Forensic;

    for esc in [true, false] {
        let (mut app, _, root) = app();
        app.prompt_state = prompt(true);
        app.prompt_state.set_prompt("abc");
        let stale = crate::record::JobId::from(999);
        job_commit(&mut app, root.id(), stale.get(), Forensic::JobOpened);
        let key = if esc {
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        };
        app.key(key, &ctx());
        app.key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &ctx(),
        );
        assert_eq!(
            app.prompt_state.prompt_text(),
            if esc { "ab" } else { "abcx" }
        );
    }
}

#[test]
fn successful_stop_with_no_recalled_text_consumes_vi_escape() {
    let (mut app, _, root) = app();
    app.prompt_state = prompt(true);
    app.prompt_state.set_prompt("abc");
    let exchange = open_exchange(&mut app, &root);
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctx());
    assert!(exchange.is_cancelled());
    app.key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &ctx(),
    );
    assert_eq!(app.prompt_state.prompt_text(), "abcx");
}
