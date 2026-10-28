use super::super::block::Chrome;
use super::*;
use crate::agent::fleet::Fleet;
use crate::agent::testkit::{TestAgent, TestAgentSpec, test_agent};

/// A root that stays resolvable while its fixture lives.
fn root(idle: Duration) -> (Arc<Fleet>, TestAgent) {
    let fleet = Fleet::for_test();
    let root = test_agent(
        &fleet,
        TestAgentSpec {
            idle,
            ..TestAgentSpec::new("main")
        },
    )
    .expect("a fresh root");
    (fleet, root)
}

/// A subagent tab whose agent has already settled — the ordinary state of a
/// tab the frontend still draws.
fn born(tabs: &mut Tabs, id: AgentId, name: &str, parent: Option<AgentId>) {
    tabs.born(id, &Born::for_test(name, parent), None);
}

#[test]
fn parent_focus_climbs_past_a_lingering_ancestor() {
    let (fleet, root) = root(Duration::ZERO);
    let mut tabs = Tabs::new(root.id, &Born::for_test("main", None), fleet, None);
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
/// only the expired view goes.
#[test]
fn tick_retires_only_the_expired_view_leaving_a_live_sibling_untouched() {
    let (fleet, root) = root(Duration::ZERO);
    let mut tabs = Tabs::new(root.id, &Born::for_test("main", None), fleet, None);
    let child = AgentId::new(root.id.get() + 1);
    born(&mut tabs, child, "child", Some(root.id));
    for (id, text) in [(child, "child says hi"), (root.id, "root says hi")] {
        tabs.scrollback_mut(id)
            .expect("both tabs have a scrollback")
            .push_chrome(Chrome::Note(text.into()));
    }
    tabs.died(child);
    // Backdate rather than wait LINGER out in a test.
    let tab = tabs.tab_mut(child).expect("the child has a tab");
    let Life::Lingering(_, at) = &mut tab.life else {
        panic!("a dead child lingers");
    };
    *at = Instant::now()
        .checked_sub(LINGER + Duration::from_secs(1))
        .expect("an instant a minute back");

    assert!(tabs.tick(), "the expiry is a repaint cue");

    assert!(
        tabs.scrollback(child).is_none(),
        "the dead child is gone once past LINGER, its scrollback with it"
    );
    assert_eq!(
        tabs.scrollback(root.id).unwrap().probe_figures().0,
        1,
        "the live root's own block survives the sibling's expiry untouched"
    );
    assert_eq!(tabs.len(), 1, "and the bar is back to root alone");
}

/// The first retirement's stamp stands: a death after a `/clear` does not
/// restart the clock.
#[test]
fn the_first_retirement_stamp_wins() {
    let (fleet, root) = root(Duration::ZERO);
    let mut tabs = Tabs::new(root.id, &Born::for_test("main", None), fleet, None);
    let child = AgentId::new(root.id.get() + 1);
    born(&mut tabs, child, "child", Some(root.id));
    tabs.retire_subtree(root.id);
    let stamp = |tabs: &Tabs| match tabs.tab(child).map(|t| &t.life) {
        Some(Life::Lingering(_, at)) => *at,
        _ => panic!("the child lingers"),
    };
    let first = stamp(&tabs);

    tabs.died(child);

    assert_eq!(stamp(&tabs), first);
}

/// Re-pinning overwrites a slot in place and `drop_pin` removes one.
#[test]
fn pins_overwrite_in_place_and_keep_insertion_order() {
    use crate::card::Mark;
    let raw = |b: &[u8]| Card(vec![Mark::Raw { bytes: b.to_vec() }]);
    let keys = |s: &Status| s.pins().iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    let mut status = Status::new();
    status.set_pin("tasks".into(), raw(b"v1"));
    status.set_pin("build".into(), raw(b"ok"));
    assert_eq!(keys(&status), ["tasks", "build"]);

    status.set_pin("tasks".into(), raw(b"v2"));
    assert_eq!(keys(&status), ["tasks", "build"]);
    assert!(matches!(&status.pins()[0].1.0[..], [Mark::Raw { bytes }] if bytes == b"v2"));

    status.drop_pin("tasks");
    assert_eq!(keys(&status), ["build"]);
}
