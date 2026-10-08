//! Session/view lifecycle — one [`Tab`] per agent the stream has announced,
//! in birth order, root first.  [`super::App`]'s `tabs` field.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::resources::ViewFigures;
use crate::agent::Agent;
use crate::agent::fleet::Fleet;
use crate::record::AgentId;

use super::block::{AgentSlot, Detail};
use super::scrollback::Scrollback;
use super::{DEMOTE_IDLE, LINGER};

/// One session's view, and the frontend's whole handle on the agent behind it.
pub(super) struct Tab {
    /// Wire identity: what every `Signal` names, so what a lookup matches on.
    id: AgentId,
    /// Birth facts, off the `Born` notice; immutable, like `Agent::parent`, and
    /// still readable once the agent has settled — which is what a lingering row's
    /// label and indentation need.
    name: String,
    /// The spawning agent, so focus can climb toward the trunk when the focused
    /// agent ends.  `None` for root and for a `/branch`, which roots its own
    /// tree.
    parent: Option<AgentId>,
    /// Retained past death and past bar expiry — tombstoned, not dropped — so
    /// `App::flush_logs` can still write this session's `user.log`.
    scrollback: Scrollback,
    /// The linger clock: the stream position of `Died`, or the `/clear`
    /// keystroke.  In the bar while `None` or younger than [`LINGER`].
    retired: Option<Instant>,
}

impl Tab {
    /// Whether the bar still shows this row: read off the scrollback, which
    /// [`Tabs::tick`] tombstones at the very moment the row goes.
    fn in_bar(&self) -> bool {
        !self.scrollback.tombstoned()
    }

    /// Frozen: still drawing its final frame, but no further event belongs in
    /// it.
    fn lingering(&self) -> bool {
        self.retired.is_some() && self.in_bar()
    }

    /// Resolved afresh each time and never stored: the frontend must not hold
    /// an agent past its avatar.  The id test rules out a namesake born later.
    fn live(&self, fleet: &Fleet) -> Option<Arc<Agent>> {
        fleet
            .resolve(&self.name)
            .filter(|agent| agent.id == self.id)
    }

    /// Idle span if this tab is parked past the compact-row threshold. Root
    /// and the focused tab never demote.
    fn demotion(&self, fleet: &Fleet, focused: AgentId, root: AgentId) -> Option<Duration> {
        if self.id == root || self.id == focused {
            return None;
        }
        let agent = self.live(fleet)?;
        let idle = agent.idle();
        (agent.mailbox.waiting_for_input() && idle >= DEMOTE_IDLE).then_some(idle)
    }
}

/// One bar row as the matrix reads it — a tab projected for a single frame,
/// nothing retained.
pub(super) struct TabRow<'a> {
    pub id: AgentId,
    pub name: &'a str,
    pub parent: Option<AgentId>,
    pub sb: &'a Scrollback,
    pub lingering: bool,
    pub demoted: Option<Duration>,
}

/// Every tab the stream has announced, in birth order.
///
/// Focus is purely presentational: matrix `Enter` and `/focus` write it, and
/// no agent-side lifecycle reads it. [`Self::focused`] resolves a stale id —
/// a tab that aged out — to root.
#[allow(clippy::struct_field_names)] // `tabs` is the natural name for the tab list.
pub(super) struct Tabs {
    /// Birth order, root first — stable per-session log paths across runs.
    /// Root is `tabs[0]` and is never retired.
    tabs: Vec<Tab>,
    focus: AgentId,
    /// The rung a group's deliberation reads at across every view —
    /// `/thinking`'s datum, kept here because it outlives any one scrollback: a
    /// tab born after the command was typed inherits it.
    thinking: Detail,
    title_frame: u64,
    /// Resolves a tab's name to its live agent.
    fleet: Arc<Fleet>,
}

impl Tabs {
    pub fn new(root: &Arc<Agent>, fleet: Arc<Fleet>, append: bool) -> Self {
        // Born collapsed: deliberation reads as its grain and bulk, and `/thinking`
        // opens the text for whoever wants it.
        let thinking = Detail::Summary;
        Self {
            tabs: vec![Tab {
                id: root.id,
                name: root.name.clone(),
                parent: None,
                scrollback: Scrollback::new(
                    root.log_dir.join("user.log"),
                    AgentSlot::default(),
                    append,
                    thinking,
                ),
                retired: None,
            }],
            focus: root.id,
            thinking,
            title_frame: 0,
            fleet,
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
        self.tab(self.focused())
            .map_or("?", |tab| tab.name.as_str())
    }

    /// The live agent behind `id`, for one handler's duration — the frontend's
    /// one door onto an agent, and never held past the statement that opens it.
    pub(super) fn agent(&self, id: AgentId) -> Option<Arc<Agent>> {
        self.tab(id)?.live(&self.fleet)
    }

    pub(super) fn focused_agent(&self) -> Option<Arc<Agent>> {
        self.agent(self.focused())
    }

    /// The live tab named `name` — `/focus`'s target. `Fleet::enrol` keeps
    /// names unique among the live, so a lingering tab sharing its name with
    /// a newborn is ruled out by the liveness test rather than by luck.
    pub(super) fn by_name(&self, name: &str) -> Option<AgentId> {
        self.tabs
            .iter()
            .find(|t| t.name == name && t.in_bar() && t.live(&self.fleet).is_some())
            .map(|t| t.id)
    }

    /// Nearest still-attended ancestor tab of `id`, else root: where focus lands
    /// when the focused agent ends.  A lingering intermediate is climbed past,
    /// not landed on.
    pub(super) fn parent_focus(&self, id: AgentId) -> AgentId {
        let mut cur = id;
        while let Some(parent) = self.tab(cur).and_then(|t| t.parent) {
            if self
                .tab(parent)
                .is_some_and(|t| t.in_bar() && !t.lingering())
            {
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
                tab.scrollback.evict_to_tombstone();
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
            name,
            parent,
            scrollback: Scrollback::new(log_dir.join("user.log"), slot, false, self.thinking),
            retired: None,
        });
    }

    /// Flip the standing rung for deliberation and apply it to every view at
    /// once — every group on screen and every one still to arrive.  Reports
    /// the rung now in force, which `/thinking` names back to the user.
    pub(super) fn toggle_thinking(&mut self) -> Detail {
        self.thinking = match self.thinking {
            Detail::Full => Detail::Summary,
            _ => Detail::Full,
        };
        for tab in &mut self.tabs {
            tab.scrollback.set_thinking_level(self.thinking);
        }
        self.thinking
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

    /// Attach to `id`. Matrix navigation selects any displayed row; `/focus`
    /// first resolves a name against the live tabs.
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

    pub(super) fn scrollback(&self, id: AgentId) -> Option<&Scrollback> {
        self.tab(id).map(|t| &t.scrollback)
    }

    pub(super) fn scrollback_mut(&mut self, id: AgentId) -> Option<&mut Scrollback> {
        self.tab_mut(id).map(|t| &mut t.scrollback)
    }

    pub(super) fn focused_scrollback(&self) -> Option<&Scrollback> {
        self.scrollback(self.focused())
    }

    /// Every view, tombstones included, in birth order — `App::flush_logs`'s
    /// stable log-path order.
    pub(super) fn views_mut(&mut self) -> impl Iterator<Item = &mut Scrollback> {
        self.tabs.iter_mut().map(|t| &mut t.scrollback)
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
                sb: &t.scrollback,
                lingering: t.lingering(),
                demoted: t.demotion(&self.fleet, focused, root),
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

    /// The ids a scrollback event can legitimately name — read only by the trace
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
mod tests;
