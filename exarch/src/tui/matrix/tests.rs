use super::*;
use crate::agent::fleet::Fleet;
use crate::agent::testkit::{TestAgentSpec, test_agent};
use ratatui::crossterm::event::KeyModifiers;
use std::path::Path;
use std::sync::Arc;

fn tree() -> (Arc<crate::agent::Agent>, super::super::tabs::Tabs) {
    let fleet = Fleet::for_test();
    let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("fresh trunk");
    let mut tabs = super::super::tabs::Tabs::new(&root, fleet, false);
    let a = AgentId::new(root.id.get() + 1);
    let a1 = AgentId::new(root.id.get() + 2);
    let a2 = AgentId::new(root.id.get() + 3);
    let b = AgentId::new(root.id.get() + 4);
    let b1 = AgentId::new(root.id.get() + 5);
    for (id, name, parent) in [
        (a, "a", Some(root.id)),
        (a1, "a1", Some(a)),
        (a2, "a2", Some(a)),
        (b, "b", Some(root.id)),
        (b1, "b1", Some(b)),
    ] {
        tabs.born(
            id,
            Path::new("/tmp/exarch-matrix-test"),
            name.into(),
            parent,
            super::super::block::AgentSlot(1),
        );
    }
    (root, tabs)
}

#[test]
fn prefixes_keep_full_forest_shape() {
    let (root, tabs) = tree();
    let rows = tabs.rows();
    let ordered = forest(&rows, MatrixSort::Spawn);
    let prefixes: Vec<&str> = ordered.iter().map(|row| row.prefix.as_str()).collect();
    assert_eq!(
        prefixes,
        ["", "├─ ", "│  ├─ ", "│  └─ ", "└─ ", "   └─ "],
        "connectors describe siblings and descendants, not just indentation"
    );
    assert_eq!(rows[ordered[0].index].id, root.id);
}

#[test]
fn the_view_holds_the_cursor_and_fills_its_lines() {
    for total in 0..24 {
        for height in 0..12 {
            for cursor in 0..total {
                let v = view(total, height, cursor);
                let at = format!("view({total}, {height}, {cursor})");
                if height > 0 {
                    assert!(v.rows.contains(&cursor), "{at} lost the cursor");
                }
                let lines = v.rows.len() + usize::from(v.above > 0) + usize::from(v.below > 0);
                assert!(lines <= height, "{at} asks for {lines} of {height} lines");
                if total > height {
                    assert_eq!(lines, height, "{at} leaves a line of the strip blank");
                }
                for (stated, hidden, side) in [
                    (v.above, v.rows.start, "above"),
                    (v.below, total - v.rows.end, "below"),
                ] {
                    assert!(
                        stated == 0 || stated == hidden,
                        "{at} claims {stated} rows {side}, not {hidden}"
                    );
                }
            }
        }
    }
}

#[test]
fn neighbour_walks_the_drawn_order_and_stops_at_the_ends() {
    let (root, tabs) = tree();
    let rows = tabs.rows();
    let step = |id, down| neighbour(&rows, MatrixSort::Spawn, id, down);

    assert_eq!(step(root.id, false), root.id, "the first row is the top");
    let mut id = root.id;
    for n in 1..=5 {
        id = step(id, true);
        assert_eq!(
            id,
            AgentId::new(root.id.get() + n),
            "down walks the drawn order"
        );
    }
    assert_eq!(step(id, true), id, "the last row is the bottom");
    assert_eq!(
        step(id, false),
        AgentId::new(root.id.get() + 4),
        "up walks it back"
    );
}

#[test]
fn nav_reads_esc_only_while_the_matrix_is_up() {
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    assert_eq!(nav(&key(KeyCode::Tab), false), Some(Nav::Toggle));
    assert_eq!(nav(&key(KeyCode::Tab), true), Some(Nav::Toggle));
    for (code, gesture) in [
        (KeyCode::Up, Nav::Up),
        (KeyCode::Down, Nav::Down),
        (KeyCode::BackTab, Nav::Up),
        (KeyCode::Enter, Nav::Attach),
        (KeyCode::Esc, Nav::Leave),
    ] {
        assert_eq!(nav(&key(code), true), Some(gesture));
        assert_eq!(
            nav(&key(code), false),
            None,
            "{code:?} belongs to the matrix only while it is up"
        );
    }
    assert_eq!(
        nav(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), true),
        Some(Nav::Up),
        "Shift-Tab arrives as BackTab carrying its modifier"
    );
    for k in [
        KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Down, KeyModifiers::ALT),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
    ] {
        assert_eq!(
            nav(&k, true),
            None,
            "a modified {:?} is not a gesture",
            k.code
        );
    }
}
