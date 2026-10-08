use super::*;
use crate::agent::testkit::{TestAgentSpec, test_agent};
use crate::card::{Card, Mark};
use crate::tui::palette::READ_W;
use crate::tui::row::Row;
use ral_core::types::{Observation, Observed};

/// The trunk is returned alongside its `App` because the frontend resolves
/// it through the fleet only: dropping it here would settle the agent mid-test.
fn app() -> (App, BusReceiver, Arc<Agent>) {
    let (_tx, rx) = crate::bus::channel();
    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh trunk");
    let app = App::new(&root, fleet, false, false, Inbox::new());
    (app, rx, root)
}

fn text(app: &mut App, id: AgentId) -> String {
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
fn matrix_cursor_does_not_change_focus_until_enter() {
    let (mut app, rx, root) = app();
    let child = AgentId::new(root.id.get() + 1);
    app.transient(
        child,
        Transient::Born {
            log_dir: std::env::temp_dir(),
            name: "child".into(),
            parent: Some(root.id),
        },
        &rx,
    );

    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.key(key(KeyCode::Tab));
    assert!(app.matrix_navigating(), "Tab enters the matrix");
    assert_eq!(
        app.matrix_cursor(&app.tabs.rows()),
        Some(root.id),
        "the cursor starts on the attached agent"
    );

    app.key(key(KeyCode::Down));
    assert_eq!(
        app.matrix_cursor(&app.tabs.rows()),
        Some(child),
        "arrows move the matrix cursor"
    );
    assert_eq!(
        app.tabs.focused(),
        root.id,
        "moving the cursor does not retarget the prompt"
    );

    app.key(key(KeyCode::Enter));
    assert_eq!(app.tabs.focused(), child, "Enter attaches to the cursor");
    assert!(!app.matrix_navigating(), "and leaves the matrix");
}

#[test]
fn esc_leaves_the_matrix_without_moving_focus() {
    let (mut app, rx, root) = app();
    app.transient(
        AgentId::new(root.id.get() + 1),
        Transient::Born {
            log_dir: std::env::temp_dir(),
            name: "child".into(),
            parent: Some(root.id),
        },
        &rx,
    );

    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.key(key(KeyCode::Tab));
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Esc));

    assert!(!app.matrix_navigating(), "Esc leaves the surface");
    assert_eq!(
        app.tabs.focused(),
        root.id,
        "an abandoned cursor attaches to nothing"
    );
}

#[test]
fn matrix_mode_swallows_prompt_editing() {
    let (mut app, rx, root) = app();
    app.transient(
        AgentId::new(root.id.get() + 1),
        Transient::Born {
            log_dir: std::env::temp_dir(),
            name: "child".into(),
            parent: Some(root.id),
        },
        &rx,
    );
    app.prompt_state.set_prompt("draft");
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
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
    use ral_core::fact::Read;
    use ral_core::first_order::datum::Datum as _;

    let (mut app, rx, root) = app();
    let mut seq = 0;
    let mut fact = |app: &mut App, display| {
        seq += 1;
        app.fact(
            root.id,
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
        root.id,
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

    let sb = app.tabs.scrollback(root.id).expect("root has a scrollback");
    assert_eq!(
        sb.probe_figures().0,
        1,
        "the call and the two reads it produced are one block"
    );
    assert_eq!(
        sb.pins()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["tasks"],
        "the pin lands in the register, never in scrollback"
    );
    let all = text(&mut app, root.id);
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
    let helper = AgentId::new(root.id.get() + 1);
    app.transient(
        helper,
        Transient::Born {
            log_dir: std::env::temp_dir(),
            name: "helper".into(),
            parent: Some(root.id),
        },
        &rx,
    );
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
