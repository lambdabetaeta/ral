use super::*;
use crate::agent::testkit::{TestAgent, TestAgentSpec, test_agent};
use crate::card::Mark;
use crate::tui::palette::READ_W;
use crate::tui::row::Row;
use ral_core::shell::runtime::{Observation, Observed};

/// The root is returned alongside its `App` because the frontend resolves
/// it through the fleet only: dropping it here would settle the agent mid-test.
pub(in crate::tui) fn app() -> (App, BusReceiver, TestAgent) {
    app_born(&Born::for_test("main", None))
}

/// [`app`] over a root announced as `born`.
pub(in crate::tui) fn app_born(born: &Born) -> (App, BusReceiver, TestAgent) {
    let (_tx, rx) = crate::bus::channel();
    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh root");
    let app = App::new(
        root.id(),
        born,
        fleet,
        UsageMeter::default(),
        false,
        Card(Vec::new()),
        mpsc::channel().0,
    );
    (app, rx, root)
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

#[test]
fn live_vi_completion_escape_undo_and_redo_restore_the_partial_command() {
    let (mut app, _, _root) = app();
    app.prompt_state = PromptState::new(true);
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
fn escape_and_ctrl_c_interrupt_only_the_focused_exchange() {
    use crate::bus::{Item, Next, ParkMode};
    use ral_core::process::CancelCause;

    for key in [
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let fleet = Fleet::for_test();
        let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh root");
        let mut app = App::new(
            root.id(),
            &Born::for_test("main", None),
            fleet.clone(),
            UsageMeter::default(),
            false,
            Card(Vec::new()),
            mpsc::channel().0,
        );
        let child = test_agent(
            &fleet,
            TestAgentSpec::child("child", &root, root.inbox.mailbox()),
        )
        .expect("a child in the root's work");
        root.steer("first".into()).unwrap();
        assert!(matches!(
            root.inbox.draw(|_| ParkMode::Quiesce),
            Some(Next::Item(Item::Human(text))) if text == "first"
        ));
        root.inbox.open_exchange();
        let exchange = root.inbox.exchange();

        app.key(key, &ctx());

        assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
        assert!(root.is_live() && child.is_live());
        root.steer("next".into()).unwrap();
        assert!(matches!(
            root.inbox.draw(|_| ParkMode::Quiesce),
            Some(Next::Item(Item::Human(text))) if text == "next"
        ));
        root.inbox.open_exchange();
        assert!(!root.inbox.exchange().is_cancelled());
        assert!(child.is_live());
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
fn esc_leaves_the_matrix_without_moving_focus() {
    let (mut app, _, root) = app();
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
    use crate::record::{Display, Locus, Record, Recorded, Seq};
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
        app.fact(
            root.id(),
            &Recorded::new(Locus::placeholder(Seq::new(seq)), Record::Display(display)),
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
    use crate::record::{Display, Locus, Record, Recorded, Seq};

    let (mut app, rx, root) = app();
    let helper = AgentId::new(root.id().get() + 1);
    born(&mut app, helper, "helper", root.id());
    let locus = Locus::placeholder(Seq::new(1));
    app.fact(
        helper,
        &Recorded::new(
            locus,
            Record::Display(Display::Answer {
                text: "alive".into(),
            }),
        ),
    );
    app.transient(helper, Transient::Died, &rx);
    let blocks = app
        .tabs
        .scrollback(helper)
        .expect("the child keeps its scrollback through the linger window")
        .probe_figures()
        .0;

    let locus = Locus::placeholder(Seq::new(2));
    app.fact(
        helper,
        &Recorded::new(
            locus,
            Record::Display(Display::Answer {
                text: "straggler".into(),
            }),
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
        false,
        Card(Vec::new()),
        mpsc::channel().0,
    );
    let hear = |app: &mut App| {
        while let Ok(Signal::Fact(_, rec)) = rx.try_recv() {
            app.fact(root.id(), &rec);
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
    assert!(status.state.state == AgentState::Evaluating && !status.pins().is_empty());

    let _recorded = recorder.emit(Protocol::Cleared).expect("the log appends");
    hear(&mut app);

    let status = app.tabs.status(root.id()).expect("root has a status");
    assert!(
        status.state.state == AgentState::Ready && status.pins().is_empty(),
        "the clear wiped both"
    );
}

/// Enter in `/rewind` rewinds the tab the overlay was opened on, whichever
/// tab has focus by then.
#[test]
fn rewind_acts_on_the_tab_it_was_opened_on() {
    use crate::bus::{Next, ParkMode, Rewrite};
    use crate::record::{Locus, Protocol, Seq, Voice};

    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh root");
    let mut app = App::new(
        root.id(),
        &Born::for_test("main", None),
        fleet,
        UsageMeter::default(),
        false,
        Card(Vec::new()),
        mpsc::channel().0,
    );
    let child = AgentId::new(root.id().get() + 1);
    born(&mut app, child, "child", root.id());
    app.fact(
        root.id(),
        &Recorded::new(
            Locus::placeholder(Seq::new(1)),
            Record::Protocol(Protocol::UserPrompt {
                turn: 1,
                text: "hello".into(),
                voice: Voice::Human,
            }),
        ),
    );

    rewind::rewind(&mut app);
    app.tabs.set_focus(child);
    app.overlay_input(&Input::Key(KeyCode::Enter), &ctx());

    assert!(app.overlay.is_none(), "Enter closes the overlay");
    assert!(
        matches!(
            root.inbox.draw(|_| ParkMode::Quiesce),
            Some(Next::Rewrite(Rewrite::Rewind(1)))
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
