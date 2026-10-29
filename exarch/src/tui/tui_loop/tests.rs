use super::{Pass, drain_signals};
use crate::agent::spawn::hatch_root;
use crate::agent::testkit::{self, scripted};
use crate::host::{Admission, HELPER_IDLE, Host, RootConfig, RootEngine, Trunk};
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
    let host = testkit::host(testkit::trunk(Trunk::Converses));
    let mut app = app_for_host(&host);
    let root = app.tabs.focused();

    assert_eq!(submit(&mut app, "/quit"), Some(Handoff::Quit));
    assert!(host.port().is_live(root));
    assert!(!app.tabs.settled());
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn settlement_waits_for_the_last_root_including_a_new_root_born_before_its_creator_dies() {
    let first = testkit::trunk(Trunk::Converses);
    let second = hatch_root(&first, Memory::Amnemon, Some("second".into())).unwrap();
    let host = Host::adopt(vec![(first, None), (second, None)]).unwrap();
    let [first, second] = *host.roots() else {
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
    let config = || RootConfig {
        system: "system".into(),
        run_dir: (*run).clone(),
        trunk: Trunk::Converses,
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
            host.roots()[0],
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
    let [first, second] = *host.roots() else {
        panic!("both recorded roots resume")
    };
    let mut app = app_for_host(&host);
    host.port().end(first).unwrap();
    let mut saw_second = false;
    let mut saw_death = false;
    let deadline = Instant::now() + WAIT;
    loop {
        let pass = drain_signals(host.feed(), None, |signal| {
            if let Signal::Born(id, born) = &signal
                && *id == second
            {
                assert!(born.resumed.is_some(), "this is an actual resumed root");
                saw_second = true;
            }
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
        if saw_death {
            break;
        }
        assert!(Instant::now() < deadline, "the first root publishes Died");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(host.end(), [] as [String; 0]);
}
