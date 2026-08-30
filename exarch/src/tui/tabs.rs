//! Session/view lifecycle — one [`Tab`] per agent the stream has announced,
//! in birth order, root first.  [`super::App`]'s `tabs` field.

use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crate::agent::Agent;
use crate::agent::resources::ViewFigures;
use crate::bus::AgentId;

use super::block::{AgentSlot, Reveal};
use super::viewport::Viewport;
use super::{DEMOTE_IDLE, LINGER};

/// One session's view, and the frontend's whole handle on the agent behind it.
pub(super) struct Tab {
    /// Wire identity: what every `Signal` names, so what a lookup matches on.
    id: AgentId,
    /// Reach.  Upgraded for the duration of one handler and never stored
    /// strong: the frontend must not hold an agent past its avatar.
    agent: Weak<Agent>,
    /// Birth facts, off the `Born` notice; immutable, like `Agent::parent`, and
    /// still readable once the `Weak` is dead — which is what a lingering row's
    /// label and indentation need.
    name: String,
    /// The spawning agent, so focus can climb toward the trunk when the focused
    /// agent ends.  `None` for root and for a `/branch`, which roots its own
    /// tree.
    parent: Option<AgentId>,
    /// Retained past death and past bar expiry — tombstoned, not dropped — so
    /// `App::flush_logs` can still write this session's `user.log`.
    viewport: Viewport,
    /// The linger clock: the stream position of `Died`, or the `/clear`
    /// keystroke.  In the bar while `None` or younger than [`LINGER`].
    retired: Option<Instant>,
}

impl Tab {
    /// Whether the bar still shows this row: read off the viewport, which
    /// [`Tabs::tick`] tombstones at the very moment the row goes.
    fn in_bar(&self) -> bool {
        !self.viewport.tombstoned()
    }

    /// Frozen: still drawing its final frame, but no further event belongs in
    /// it.
    fn lingering(&self) -> bool {
        self.retired.is_some() && self.in_bar()
    }

    fn live(&self) -> Option<Arc<Agent>> {
        self.agent.upgrade()
    }

    /// Idle span if this tab is due out of the `TAB` cycle — parked and idle
    /// past the mark — else `None`.  Root and the focused tab never are, which
    /// is what makes [`Tabs::focus_next`]'s walk terminate.
    fn demotion(&self, focused: AgentId, root: AgentId) -> Option<Duration> {
        if self.id == root || self.id == focused {
            return None;
        }
        let agent = self.live()?;
        let idle = agent.idle();
        (agent.mailbox().waiting_for_input() && idle >= DEMOTE_IDLE).then_some(idle)
    }
}

/// One bar row as the matrix reads it — a tab projected for a single frame,
/// nothing retained.
pub(super) struct TabRow<'a> {
    pub id: AgentId,
    pub name: &'a str,
    pub parent: Option<AgentId>,
    pub vp: &'a Viewport,
    pub lingering: bool,
    pub demoted: Option<Duration>,
}

/// Every tab the stream has announced, in birth order.
///
/// Focus is purely presentational: `TAB` and `/focus` write it, rendering reads
/// it, and no agent-side lifecycle depends on it.  Reads route through
/// [`Self::focused`], which resolves a stale id — a tab that aged out — to root.
#[allow(clippy::struct_field_names)] // `tabs` is the natural name for the tab list.
pub(super) struct Tabs {
    /// Birth order, root first — stable per-session log paths across runs.
    /// Root is `tabs[0]` and is never retired.
    tabs: Vec<Tab>,
    focus: AgentId,
    /// The rung thinking traces read at across every view — `/thinking`'s
    /// datum, kept here because it outlives any one viewport: a tab born after
    /// the command was typed inherits it.
    traces: Reveal,
    title_frame: u64,
}

impl Tabs {
    pub fn new(root: &Arc<Agent>, append: bool) -> Self {
        let traces = Reveal::Full;
        Self {
            tabs: vec![Tab {
                id: root.id,
                agent: Arc::downgrade(root),
                name: root.name().to_string(),
                parent: None,
                viewport: Viewport::new(
                    root.log_dir().join("user.log"),
                    AgentSlot::default(),
                    append,
                    traces,
                ),
                retired: None,
            }],
            focus: root.id,
            traces,
            title_frame: 0,
        }
    }

    fn tab(&self, id: AgentId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: AgentId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub(super) fn root(&self) -> AgentId {
        self.tabs[0].id
    }

    /// The focused tab, resolving a stale focus — a subagent that aged out of
    /// the bar — to root.
    pub(super) fn focused(&self) -> AgentId {
        match self.tab(self.focus) {
            Some(tab) if tab.in_bar() => self.focus,
            _ => self.root(),
        }
    }

    /// The focused tab's label, for the watch-only prompt hint.
    pub(super) fn focused_name(&self) -> &str {
        self.tab(self.focused()).map_or("?", |tab| tab.name.as_str())
    }

    /// The live agent behind `id`, for one handler's duration — the frontend's
    /// one door onto an agent, and never held past the statement that opens it.
    pub(super) fn agent(&self, id: AgentId) -> Option<Arc<Agent>> {
        self.tab(id)?.live()
    }

    pub(super) fn focused_agent(&self) -> Option<Arc<Agent>> {
        self.agent(self.focused())
    }

    /// The live tab named `name` — `/focus`'s target, and the only way onto a
    /// demoted tab.  `Fleet::enrol` keeps names unique among the live, so a
    /// lingering tab sharing its name with a newborn is ruled out by the
    /// liveness test rather than by luck.
    pub(super) fn by_name(&self, name: &str) -> Option<AgentId> {
        self.tabs
            .iter()
            .find(|t| t.name == name && t.in_bar() && t.live().is_some())
            .map(|t| t.id)
    }

    /// Nearest still-attended ancestor tab of `id`, else root: where focus lands
    /// when the focused agent ends.  A lingering intermediate is climbed past,
    /// not landed on.
    pub(super) fn parent_focus(&self, id: AgentId) -> AgentId {
        let mut cur = id;
        while let Some(parent) = self.tab(cur).and_then(|t| t.parent) {
            if self.tab(parent).is_some_and(|t| t.in_bar() && !t.lingering()) {
                return parent;
            }
            cur = parent;
        }
        self.root()
    }

    /// Age retired tabs out past [`LINGER`], once per frame.  An expired view is
    /// evicted to a tombstone rather than dropped: the tab must survive for the
    /// `/resources` dead-view count and `App::flush_logs`'s log-path listing.
    /// Returns whether a tab went — the cue to repaint.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let expired: Vec<AgentId> = self
            .tabs
            .iter()
            .filter(|t| t.in_bar())
            .filter_map(|t| (now.duration_since(t.retired?) >= LINGER).then_some(t.id))
            .collect();
        let changed = !expired.is_empty();
        for id in expired {
            if let Some(tab) = self.tab_mut(id) {
                tab.viewport.evict_to_tombstone();
            }
            if self.focus == id {
                self.focus = self.parent_focus(id);
            }
        }
        self.title_frame += 1;
        changed
    }

    /// Open a tab for an agent the stream has just announced.  A second `Born`
    /// for an id already listed is ignored: a tab is opened once, and its birth
    /// facts never change.
    pub(super) fn born(
        &mut self,
        id: AgentId,
        agent: Weak<Agent>,
        log_dir: &Path,
        name: String,
        parent: Option<AgentId>,
        slot: AgentSlot,
    ) {
        if self.tab(id).is_some() {
            return;
        }
        self.tabs.push(Tab {
            id,
            agent,
            name,
            parent,
            viewport: Viewport::new(log_dir.join("user.log"), slot, false, self.traces),
            retired: None,
        });
    }

    /// Flip the standing rung for thinking traces and apply it to every view at
    /// once — every trace on screen and every one still to arrive.  Reports the
    /// rung now in force, which `/thinking` names back to the user.
    pub(super) fn toggle_traces(&mut self) -> Reveal {
        self.traces = match self.traces {
            Reveal::Full => Reveal::Summary,
            _ => Reveal::Full,
        };
        for tab in &mut self.tabs {
            tab.viewport.set_traces_level(self.traces);
        }
        self.traces
    }

    /// Start the linger clock at this `Died`'s position in the stream.  Root
    /// never enters the window; it outlives the session.
    pub(super) fn died(&mut self, id: AgentId) {
        if id == self.root() {
            return;
        }
        let now = Instant::now();
        if let Some(tab) = self.tab_mut(id) {
            tab.retired = Some(now);
        }
        if self.focus == id {
            self.focus = self.parent_focus(id);
        }
    }

    /// Retire every non-root tab into the linger window — `/clear`.  A tab
    /// already retired keeps its earlier stamp, so a child that died just before
    /// the clear is not given a fresh full window.
    pub(super) fn retire_all(&mut self) {
        let (now, root) = (Instant::now(), self.root());
        for tab in self.tabs.iter_mut().filter(|t| t.id != root && t.in_bar()) {
            if tab.retired.is_none() {
                tab.retired = Some(now);
            }
        }
        self.focus = root;
    }

    /// Cycle `TAB` focus past any demoted tab.  Root is never demoted, so one
    /// pass around the bar always finds a landing spot.
    pub(super) fn focus_next(&mut self) {
        let (current, root) = (self.focused(), self.root());
        let bar: Vec<&Tab> = self.tabs.iter().filter(|t| t.in_bar()).collect();
        let pos = bar.iter().position(|t| t.id == current).unwrap_or(0);
        let next = (1..=bar.len())
            .map(|step| bar[(pos + step) % bar.len()])
            .find(|t| t.demotion(current, root).is_none())
            .map(|t| t.id);
        if let Some(id) = next {
            self.focus = id;
        }
    }

    /// Land focus on `id` — `/focus`, the only way to reach a demoted tab.  No
    /// validation: [`Self::by_name`] resolved it against the live tabs.
    pub(super) fn set_focus(&mut self, id: AgentId) {
        self.focus = id;
    }

    /// Whether `id` is a `/branch` tab — the only kind `/close` may kill.  A
    /// branch is exactly a spawned tab that roots its own tree, which is a
    /// birth fact and so still answerable once the agent has gone.
    pub(super) fn is_branch(&self, id: AgentId) -> bool {
        id != self.root() && self.tab(id).is_some_and(|t| t.parent.is_none())
    }

    /// Whether `id`'s tab is frozen in its linger window, so no further event
    /// belongs in it.
    pub(super) fn lingering(&self, id: AgentId) -> bool {
        self.tab(id).is_some_and(Tab::lingering)
    }

    pub(super) fn viewport(&self, id: AgentId) -> Option<&Viewport> {
        self.tab(id).map(|t| &t.viewport)
    }

    pub(super) fn viewport_mut(&mut self, id: AgentId) -> Option<&mut Viewport> {
        self.tab_mut(id).map(|t| &mut t.viewport)
    }

    pub(super) fn focused_viewport(&self) -> Option<&Viewport> {
        self.viewport(self.focused())
    }

    /// Every view, tombstones included, in birth order — `App::flush_logs`'s
    /// stable log-path order.
    pub(super) fn views_mut(&mut self) -> impl Iterator<Item = &mut Viewport> {
        self.tabs.iter_mut().map(|t| &mut t.viewport)
    }

    /// Rows in the bar — not the tab count, which outlives them.
    pub(super) fn len(&self) -> usize {
        self.tabs.iter().filter(|t| t.in_bar()).count()
    }

    /// This frame's bar rows, each tab projected as the matrix reads it.
    pub(super) fn rows(&self) -> Vec<TabRow<'_>> {
        let (focused, root) = (self.focused(), self.root());
        self.tabs
            .iter()
            .filter(|t| t.in_bar())
            .map(|t| TabRow {
                id: t.id,
                name: &t.name,
                parent: t.parent,
                vp: &t.viewport,
                lingering: t.lingering(),
                demoted: t.demotion(focused, root),
            })
            .collect()
    }

    /// The `/resources` view census.  A retired tab is dead whether it is still
    /// lingering in the bar or already tombstoned — `retired` is never cleared,
    /// and nothing but a retired tab is ever tombstoned.
    pub(super) fn census(&self) -> ViewFigures {
        let dead = self.tabs.iter().filter(|t| t.retired.is_some()).count() as u64;
        let live = self.tabs.len() as u64 - dead;
        ViewFigures {
            live,
            dead,
            agents: live,
        }
    }

    /// The ids a viewport event can legitimately name — read only by the trace
    /// that reports a dropped one.
    #[cfg(debug_assertions)]
    pub(super) fn ids(&self) -> Vec<AgentId> {
        self.tabs.iter().map(|t| t.id).collect()
    }

    /// Frame counter driving the terminal tab-title spinner.
    pub(super) fn title_frame(&self) -> u64 {
        self.title_frame
    }
}

#[cfg(test)]
mod tests {
    use super::super::block::ChromeKind;
    use super::*;
    use crate::agent::testkit::{TestAgentSpec, test_agent};
    use crate::fleet::Fleet;
    use ratatui::text::Line;

    /// A trunk whose `Weak` upgrades for as long as the returned `Arc` lives,
    /// which is the whole of each test.
    fn trunk(idle: Duration) -> Arc<Agent> {
        let fleet = Fleet::new();
        test_agent(
            &fleet,
            TestAgentSpec {
                idle,
                ..TestAgentSpec::new("main")
            },
        )
        .expect("a fresh trunk")
    }

    /// A subagent tab whose agent has already settled — the ordinary state of a
    /// tab the frontend still draws.
    fn born(tabs: &mut Tabs, id: AgentId, name: &str, parent: Option<AgentId>) {
        tabs.born(
            id,
            Weak::new(),
            Path::new("/tmp/exarch-tabs-test"),
            name.into(),
            parent,
            AgentSlot(1),
        );
    }

    #[test]
    fn parent_focus_climbs_past_a_lingering_ancestor() {
        let root = trunk(Duration::ZERO);
        let mut tabs = Tabs::new(&root, false);
        let (child, grandchild) = (root.id + 1, root.id + 2);
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
        let root = trunk(Duration::ZERO);
        let mut tabs = Tabs::new(&root, false);
        let child = root.id + 1;
        born(&mut tabs, child, "child", Some(root.id));
        for (id, text) in [(child, "child says hi"), (root.id, "root says hi")] {
            tabs.viewport_mut(id)
                .expect("both tabs have a viewport")
                .push_chrome(ChromeKind::Plain, vec![Line::from(text)]);
        }
        tabs.died(child);
        // Backdate rather than wait LINGER out in a test.
        tabs.tab_mut(child).expect("the child has a tab").retired =
            Instant::now().checked_sub(LINGER + Duration::from_secs(1));

        assert!(tabs.tick(), "the expiry is a repaint cue");

        assert_eq!(
            tabs.viewport(child).unwrap().probe_figures().0,
            0,
            "the dead child is tombstoned once past LINGER, its scrollback gone"
        );
        assert_eq!(
            tabs.viewport(root.id).unwrap().probe_figures().0,
            1,
            "the live root's own block survives the sibling's tombstoning untouched"
        );
        assert_eq!(tabs.len(), 1, "and the bar is back to root alone");
    }

    /// Root is never demoted, so `TAB` wraps back to it rather than sticking on
    /// the demoted tab it skips.
    #[test]
    fn focus_next_skips_a_demoted_tab() {
        let root = trunk(Duration::ZERO);
        // Idle past the mark and parked on a fresh mailbox: demoted on sight.
        let parked = trunk(DEMOTE_IDLE + Duration::from_secs(1));
        let mut tabs = Tabs::new(&root, false);
        let promoted = parked.id + 1;
        born(&mut tabs, promoted, "promoted", Some(root.id));
        tabs.born(
            parked.id,
            Arc::downgrade(&parked),
            Path::new("/tmp/exarch-tabs-test"),
            "parked".into(),
            Some(root.id),
            AgentSlot(2),
        );

        tabs.focus_next();
        assert_eq!(
            tabs.focused(),
            promoted,
            "TAB skips straight past the demoted tab"
        );

        tabs.focus_next();
        assert_eq!(
            tabs.focused(),
            root.id,
            "TAB skips the demoted tab again, wrapping back to root"
        );
    }
}
