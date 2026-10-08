use super::super::block::Chrome;
use super::*;
use crate::agent::fleet::Fleet;
use crate::agent::testkit::{TestAgentSpec, test_agent};

/// A trunk that stays resolvable for as long as the returned `Arc` lives,
/// which is the whole of each test.
fn trunk(idle: Duration) -> (Arc<Fleet>, Arc<Agent>) {
    let fleet = Fleet::for_test();
    let root = test_agent(
        &fleet,
        TestAgentSpec {
            idle,
            ..TestAgentSpec::new("main")
        },
    )
    .expect("a fresh trunk");
    (fleet, root)
}

/// A subagent tab whose agent has already settled — the ordinary state of a
/// tab the frontend still draws.
fn born(tabs: &mut Tabs, id: AgentId, name: &str, parent: Option<AgentId>) {
    tabs.born(
        id,
        Path::new("/tmp/exarch-tabs-test"),
        name.into(),
        parent,
        AgentSlot(1),
    );
}

#[test]
fn parent_focus_climbs_past_a_lingering_ancestor() {
    let (fleet, root) = trunk(Duration::ZERO);
    let mut tabs = Tabs::new(&root, fleet, false);
    let (child, grandchild) = (
        AgentId::new(root.id.get() + 1),
        AgentId::new(root.id.get() + 2),
    );
    born(&mut tabs, child, "child", Some(root.id));
    born(&mut tabs, grandchild, "grandchild", Some(child));
    tabs.died(child);

    assert_eq!(
        tabs.parent_focus(grandchild),
        root.id,
        "focus climbs past the lingering parent to the nearest attended ancestor"
    );
}

/// Killing one agent is never paid for out of a sibling's scrollback:
/// tombstoning frees only the expired view.
#[test]
fn tick_tombstones_only_the_expired_view_leaving_a_live_sibling_untouched() {
    let (fleet, root) = trunk(Duration::ZERO);
    let mut tabs = Tabs::new(&root, fleet, false);
    let child = AgentId::new(root.id.get() + 1);
    born(&mut tabs, child, "child", Some(root.id));
    for (id, text) in [(child, "child says hi"), (root.id, "root says hi")] {
        tabs.scrollback_mut(id)
            .expect("both tabs have a scrollback")
            .push_chrome(Chrome::Note(text.into()));
    }
    tabs.died(child);
    // Backdate rather than wait LINGER out in a test.
    tabs.tab_mut(child).expect("the child has a tab").retired =
        Instant::now().checked_sub(LINGER + Duration::from_secs(1));

    assert!(tabs.tick(), "the expiry is a repaint cue");

    assert_eq!(
        tabs.scrollback(child).unwrap().probe_figures().0,
        0,
        "the dead child is tombstoned once past LINGER, its scrollback gone"
    );
    assert_eq!(
        tabs.scrollback(root.id).unwrap().probe_figures().0,
        1,
        "the live root's own block survives the sibling's tombstoning untouched"
    );
    assert_eq!(tabs.len(), 1, "and the bar is back to root alone");
}
