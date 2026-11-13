use super::{Pass, drain_signals};
use crate::agent::testkit::human_child;
use crate::agent::testkit::{self, scripted};
use crate::host::{Admission, HELPER_IDLE, Host, LaunchConfig, RootEngine};
use crate::port::{Command, Read, Signal};
use crate::provider::{Bureau, Toolset, roles::Roles, scripted::Script};
use crate::record::{AgentId, Memory, Transient};
use crate::tui::App;
use crate::tui::app::Handoff;
use crate::tui::app::tests::{app_for_host, apply_signal, ctx};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ral_core::own::OwnDir;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn submit(app: &mut App, text: &str) -> Option<Handoff> {
    app.prompt_state.set_prompt(text);
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx())
}

fn until(app: &mut App, host: &Host, mut done: impl FnMut(&App, &Signal) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let signal = host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the fleet publishes the expected lifecycle signal");
        let finished = done(app, &signal);
        apply_signal(app, host.feed(), signal);
        if finished {
            return;
        }
    }
}

fn died(signal: &Signal, agent: AgentId) -> bool {
    matches!(signal, Signal::Transient(id, Transient::Died { failures }) if *id == agent && failures.is_empty())
}

#[test]
fn quit_leaves_the_real_fleet_alive_for_its_host_to_end() {
    let host = testkit::host(testkit::trunk(false));
    let mut app = app_for_host(&host);
    let root = app.tabs.focused();

    assert_eq!(submit(&mut app, "/quit"), Some(Handoff::Quit));
    assert!(host.port().is_live(root));
    assert!(!app.tabs.settled());
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn settlement_waits_for_the_last_root_including_a_new_root_born_before_its_creator_dies() {
    let first = testkit::trunk(false);
    let second = human_child(&first, Memory::Amnemon, Some("second".into())).unwrap();
    let host = Host::adopt(vec![(first, None), (second, None)]).unwrap();
    let [first, second] = *host.children().as_slice() else {
        panic!("two initial roots")
    };
    let mut app = app_for_host(&host);
    until(
        &mut app,
        &host,
        |_, signal| matches!(signal, Signal::Born(id, _) if *id == second),
    );
    assert!(!app.tabs.settled());

    submit(&mut app, "/new");
    submit(&mut app, "/close");
    let mut new_root = None;
    until(&mut app, &host, |_, signal| match signal {
        Signal::Born(id, born) if born.parent.is_none() => {
            new_root = Some(*id);
            false
        }
        signal if died(signal, first) => {
            assert!(
                new_root.is_some(),
                "the new root's birth precedes its creator's death"
            );
            true
        }
        _ => false,
    });
    let new_root = new_root.expect("/new launched a root");
    assert!(host.port().is_live(new_root));
    assert!(!app.tabs.settled());

    app.tabs.set_focus(second);
    submit(&mut app, "/close");
    until(&mut app, &host, |_, signal| died(signal, second));
    assert!(host.port().is_live(new_root));
    assert!(!app.tabs.settled(), "the /new root still owns a live tab");

    app.tabs.set_focus(new_root);
    submit(&mut app, "/close");
    until(&mut app, &host, |app, signal| {
        if died(signal, new_root) {
            assert!(
                !app.tabs.settled(),
                "even an ended root is attended until its Died is dispatched"
            );
            true
        } else {
            false
        }
    });
    assert!(app.tabs.settled());
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn drain_observes_the_queued_second_resumed_root_before_idle_and_settlement() {
    let run = OwnDir::for_test("tui-resumed-roots").unwrap();
    let config = || LaunchConfig {
        system: "system".into(),
        run_dir: (*run).clone(),
        returns: false,
        admission: Admission::Free,
        helper_idle: HELPER_IDLE,
        tools: Toolset::offered(false),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel: 0,
        search: false,
        factory: None,
        bureau: Arc::new(Bureau::scripted([])),
        max_tokens: None,
        roles: Roles::default(),
    };
    let engine = || RootEngine::Identity {
        scratch: Arc::new(crate::app::Scratch::for_test(crate::app::EXARCH, "tui-resume").unwrap()),
        cwd: ral_core::host::cwd().unwrap(),
        grants: ral_core::authority::policy::GrantStack::default(),
        terminal: ral_core::io::terminal::TerminalState::default(),
    };
    let provider = scripted("test-model", Script::new());
    let host = Host::found(config(), engine(), provider.clone()).unwrap();
    host.port()
        .command(
            host.children()[0],
            Command::Read(Read::Hatch {
                memory: Memory::Amnemon,
                name: Some("second".into()),
            }),
        )
        .unwrap();
    let mut app = app_for_host(&host);
    until(
        &mut app,
        &host,
        |_, signal| matches!(signal, Signal::Born(_, born) if born.name == "second"),
    );
    assert_eq!(host.end(), [] as [String; 0]);

    let host = Host::resume(config(), engine(), &provider).unwrap();
    let [first, second] = *host.children().as_slice() else {
        panic!("both recorded roots resume")
    };
    let mut app = app_for_host(&host);
    host.port().end(first).unwrap();
    let mut saw_second = false;
    let mut saw_death = false;
    let mut second_certified = false;
    let pass = drain_signals(host.feed(), None, |signal| {
        if let Signal::Born(id, born) = &signal
            && *id == second
        {
            assert!(born.resumed.is_some(), "this is an actual resumed root");
            saw_second = true;
        }
        second_certified |= matches!(&signal, Signal::Synced { agent, through } if *agent == second && *through > 0);
        saw_death |= died(&signal, first);
        apply_signal(&mut app, host.feed(), signal);
        Ok(())
    })
    .unwrap();
    assert!(matches!(pass, Pass::Idle));
    assert!(
        saw_second,
        "Idle cannot leave the queued second Born undispatched"
    );
    assert!(
        !app.tabs.settled(),
        "the second resumed root keeps the loop alive"
    );
    if !saw_death {
        until(&mut app, &host, |_, signal| {
            second_certified |= matches!(signal, Signal::Synced { agent, through } if *agent == second && *through > 0);
            died(signal, first)
        });
    }
    if !second_certified {
        until(
            &mut app,
            &host,
            |_, signal| matches!(signal, Signal::Synced { agent, through } if *agent == second && *through > 0),
        );
    }
    let sb = app.tabs.scrollback_mut(second).unwrap();
    sb.catch_up();
    assert!(
        sb.copy_ready(),
        "resumed history is copyable without a prompt"
    );
    assert!(sb.reader().snapshot().is_ok());
    let (_, _, mut commits) = sb.reader().since(None).unwrap();
    assert!(
        !commits.any(
            |commit| commit.unwrap().records().iter().any(|record| matches!(
                record.value(),
                crate::record::Record::Forensic(crate::record::Forensic::TurnStarted { .. })
            ))
        ),
        "resume did not start a provider turn"
    );
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn idle_context_and_terminal_suffix_are_certified_before_copy_or_death() {
    use crate::record::{Forensic, Record};

    let host = testkit::host(testkit::trunk(false));
    let mut app = app_for_host(&host);
    let root = app.tabs.focused();
    until(
        &mut app,
        &host,
        |_, signal| matches!(signal, Signal::Synced { agent, through } if *agent == root && *through > 0),
    );
    submit(&mut app, "/context");
    let mut context = None;
    until(&mut app, &host, |_, signal| match signal {
        Signal::Commit(id, commit) if *id == root => {
            for record in commit.records() {
                assert!(
                    !matches!(
                        record.value(),
                        Record::Forensic(Forensic::TurnStarted { .. })
                    ),
                    "a command needs no provider turn"
                );
                if matches!(
                    record.value(),
                    Record::Display(crate::record::Display::Context { .. })
                ) {
                    context = Some(commit.number());
                }
            }
            false
        }
        Signal::Synced { agent, through } if *agent == root => {
            context.is_some_and(|commit| *through >= commit)
        }
        _ => false,
    });
    let sb = app.tabs.focused_scrollback().unwrap();
    assert!(
        sb.copy_ready(),
        "the idle command is certified before termination or a prompt"
    );
    assert!(sb.reader().snapshot().is_ok());
    assert!(host.port().is_live(root));

    submit(&mut app, "/close");
    let (mut closed, mut ended, mut synced) = (false, None, 0);
    until(&mut app, &host, |_, signal| {
        match signal {
            Signal::Commit(id, commit) if *id == root => {
                for record in commit.records() {
                    closed |= matches!(record.value(), Record::Forensic(Forensic::SessionClosed));
                    if matches!(record.value(), Record::Forensic(Forensic::SessionEnded)) {
                        ended = Some(commit.number());
                    }
                }
            }
            Signal::Synced { agent, through } if *agent == root => synced = *through,
            signal if died(signal, root) => {
                assert!(closed);
                assert!(
                    ended.is_some_and(|commit| synced >= commit),
                    "SessionEnded is certified before Died"
                );
                return true;
            }
            _ => {}
        }
        false
    });
    assert!(
        app.tabs
            .focused_scrollback()
            .unwrap()
            .reader()
            .snapshot()
            .is_ok()
    );
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn parent_prose_is_copyable_while_its_helper_still_owes_a_report() {
    use crate::agent::testkit::{dressed, launch_child, ral_call, set_provider};
    use crate::provider::scripted::Reply;
    use crate::record::{Control, Display, Record};
    use ral_core::builtin::{BuiltinBody, BuiltinEntry};
    use std::borrow::Cow;
    use std::sync::{Mutex, mpsc};

    let (entered, ready) = mpsc::channel();
    let (release, held) = mpsc::channel();
    let held = Mutex::new(held);
    let builtins: Arc<[BuiltinEntry]> = Arc::new([BuiltinEntry::new(
        Cow::Borrowed("test-held-helper"),
        testkit::scheme_test_clear_block_forever,
        "test-only: hold a helper until released.",
        BuiltinBody::Captured(Arc::new(move |_, _, shell| {
            entered.send(()).unwrap();
            held.lock()
                .unwrap()
                .recv_timeout(WAIT)
                .expect("release helper");
            shell.poll()?;
            Ok(ral_core::ValClos::UNIT)
        })),
    )]);
    let mut parent = crate::agent::Agent::for_test_with(crate::agent::TestLaunch {
        returns: false,
        installers: dressed(move |runtime| runtime.install_captured_builtins(&builtins)),
        ..crate::agent::TestLaunch::new("system")
    })
    .unwrap();
    set_provider(
        &mut parent,
        scripted(
            "test-model",
            Script::new().then(Reply::text("waiting for help")),
        ),
    );
    // The conversing parent may finish prose without replying or settling.
    let mut helper = testkit::marshalled_child(&parent, "held-helper", "`inherit");
    set_provider(
        &mut helper,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "hold",
                "test-held-helper",
            )])),
        ),
    );
    let (emit, _feed) = crate::bus::dummy_emitter();
    let helper = launch_child(&mut parent, helper, Some("work".into()), &emit);
    ready
        .recv_timeout(WAIT)
        .expect("helper entered its real dispatch");
    let host = testkit::host(parent);
    let mut app = app_for_host(&host);
    let root = app.tabs.focused();
    submit(&mut app, "please help");
    let mut answer = None;
    until(&mut app, &host, |_, signal| match signal {
        Signal::Commit(id, commit) if *id == root => {
            for record in commit.records() {
                assert!(
                    !matches!(record.value(), Record::Control(Control::JobSettled { .. })),
                    "helper debt prevents settlement"
                );
                if matches!(record.value(), Record::Display(Display::Answer { text }) if text == "waiting for help")
                {
                    answer = Some(commit.number());
                }
            }
            false
        }
        Signal::Synced { agent, through } if *agent == root => {
            answer.is_some_and(|commit| *through >= commit)
        }
        _ => false,
    });
    assert!(
        helper.status().unwrap().owes,
        "helper is still held, not reported"
    );
    assert!(host.port().is_live(root));
    let sb = app.tabs.focused_scrollback().unwrap();
    assert!(sb.copy_ready());
    assert!(sb.reader().snapshot().is_ok());
    release.send(()).unwrap();
    assert_eq!(host.end(), [] as [String; 0]);
}
