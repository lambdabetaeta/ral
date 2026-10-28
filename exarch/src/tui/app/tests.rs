use super::*;
use crate::agent::testkit::{TestAgent, TestAgentSpec, pop, test_agent};
use crate::card::Mark;
use crate::tui::palette::READ_W;
use crate::tui::row::Row;
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
    let (_tx, rx) = crate::bus::channel();
    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh root");
    let mut app = App::new(
        root.id(),
        born,
        fleet,
        UsageMeter::default(),
        prompt(false),
        Card(Vec::new()),
        mpsc::channel().0,
    );
    app.copy_sink = Some(Box::new(|_| Ok(clipboard::Copied::Whole)));
    (app, rx, root)
}

/// An open exchange on `root`, so an interrupt becomes observable.
fn open_exchange(root: &TestAgent) -> ral_core::process::Scope {
    root.steer("busy".into()).unwrap();
    crate::agent::testkit::open_next(&root.inbox);
    root.inbox.exchange()
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
fn escape_and_ctrl_c_interrupt_only_the_focused_exchange() {
    use crate::agent::testkit::open_next;
    use crate::bus::{Item, Next};
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
            prompt(false),
            Card(Vec::new()),
            mpsc::channel().0,
        );
        let child = test_agent(
            &fleet,
            TestAgentSpec::child("child", &root, root.inbox.mailbox()),
        )
        .expect("a child in the root's work");
        let exchange = open_exchange(&root);

        app.key(key, &ctx());

        assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
        assert!(root.is_live() && child.is_live());
        root.steer("next".into()).unwrap();
        assert!(matches!(
            open_next(&root.inbox),
            Next::Item(Item::Human(text)) if text == "next"
        ));
        assert!(!root.inbox.exchange().is_cancelled());
        assert!(child.is_live());
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
    let exchange = open_exchange(&root);
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
        let exchange = open_exchange(&root);
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
    let exchange = open_exchange(&root);
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
    let exchange = open_exchange(&root);
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
    let exchange = open_exchange(&root);
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
        let exchange = open_exchange(&root);
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
        prompt(false),
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
    use crate::bus::{Next, Rewrite};
    use crate::record::{Locus, Protocol, Seq, Voice};

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
        matches!(pop(&root.inbox), Some(Next::Rewrite(Rewrite::Rewind(1)))),
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
        let exchange = open_exchange(&root);
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
