//! Session/view lifecycle — one [`Tab`] per agent the stream has announced,
//! in id order.  [`super::App`]'s `tabs` field.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::palette::AGENT_HUES;
use super::resources::ViewFigures;
use crate::agent::Agent;
use crate::agent::fleet::Fleet;
use crate::record::{AgentId, Emitter as Recorder};

use super::block::{AgentSlot, Detail};
use super::scrollback::Scrollback;
use super::{DEMOTE_IDLE, LINGER};

/// One session's view, and the frontend's whole handle on the agent behind it.
pub(super) struct Tab {
    /// Wire identity: what every `Signal` names, so what a lookup matches on.
    id: AgentId,
    /// Birth facts, off the `Born` notice; immutable, like an agent's `Role`, and
    /// still readable once the agent has settled — which is what a lingering row's
    /// label and indentation need.
    name: String,
    /// The spawning agent, so focus can climb toward it when the focused agent
    /// ends.  `None` for a root.
    parent: Option<AgentId>,
    /// The agent's own record seam, off `Born`: how a command the frontend
    /// owns (`/model`, `/limits`) is recorded in this tab's log.
    recorder: Recorder,
    /// Retained past death and past bar expiry — tombstoned, not dropped — so
    /// `App::flush_logs` can still write this session's `user.log`.
    scrollback: Scrollback,
    /// The linger clock: the stream position of `Died`, or of the `Cleared`
    /// that retired this tab's creator.  In the bar while `None` or younger
    /// than [`LINGER`].
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

    /// Idle span if this tab is parked past the compact-row threshold. The
    /// focused tab never demotes.
    fn demotion(&self, fleet: &Fleet, focused: AgentId) -> Option<Duration> {
        if self.id == focused {
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

/// Every tab the stream has announced, in id order; never empty once the
/// first `Born` has landed.
///
/// Focus is purely presentational: matrix `Enter` and `/focus` write it, and
/// no agent-side lifecycle reads it. [`Self::focused`] resolves a stale id —
/// a tab that aged out — to the first tab in the bar.
#[allow(clippy::struct_field_names)] // `tabs` is the natural name for the tab list.
pub(super) struct Tabs {
    /// Id order: the one order `App::flush_logs` lists paths in, and the
    /// matrix's row order, across runs.
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
    /// No tabs yet, focused on `first`, whose `Born` must be the next to land.
    pub fn new(first: AgentId, fleet: Arc<Fleet>) -> Self {
        Self {
            tabs: Vec::new(),
            focus: first,
            // Born collapsed: deliberation reads as its grain and bulk, and
            // `/thinking` opens the text for whoever wants it.
            thinking: Detail::Summary,
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

    /// The first tab in the bar, else the first tab: where a stale id lands.
    fn fallback(&self) -> AgentId {
        self.tabs
            .iter()
            .find(|t| t.in_bar())
            .or_else(|| self.tabs.first())
            .map_or(self.focus, |t| t.id)
    }

    /// The focused tab, resolving a stale focus — a tab that aged out of the
    /// bar — to [`Self::fallback`].
    pub(super) fn focused(&self) -> AgentId {
        match self.tab(self.focus) {
            Some(tab) if tab.in_bar() => self.focus,
            _ => self.fallback(),
        }
    }

    /// The tab's ancestors, nearest first.
    fn ancestors(&self, id: AgentId) -> impl Iterator<Item = AgentId> {
        std::iter::successors(self.tab(id).and_then(|t| t.parent), |&p| {
            self.tab(p).and_then(|t| t.parent)
        })
    }

    /// The root of `id`'s tree — itself, if it is one.
    pub(super) fn root_of(&self, id: AgentId) -> AgentId {
        self.ancestors(id).last().unwrap_or(id)
    }

    pub(super) fn fleet(&self) -> &Fleet {
        &self.fleet
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

    /// The focused tab's own record seam.
    pub(super) fn focused_recorder(&self) -> &Recorder {
        &self
            .tab(self.focused())
            .expect("the focused tab always exists")
            .recorder
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

    /// Nearest still-attended ancestor tab of `id`, else [`Self::fallback`]:
    /// where focus lands when the focused agent ends.  A lingering intermediate
    /// is climbed past, not landed on.
    pub(super) fn parent_focus(&self, id: AgentId) -> AgentId {
        self.ancestors(id)
            .find(|&a| self.tab(a).is_some_and(|t| t.in_bar() && !t.lingering()))
            .unwrap_or_else(|| self.fallback())
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

    /// Open a tab for an agent the stream has just announced, `append`ing to
    /// its `user.log` when it is resumed.  A second `Born` for an id already
    /// listed is ignored: a tab is opened once, and its birth facts never
    /// change.
    pub(super) fn born(
        &mut self,
        id: AgentId,
        log_dir: &Path,
        name: String,
        parent: Option<AgentId>,
        recorder: Recorder,
        append: bool,
    ) {
        if self.tab(id).is_some() {
            return;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "modulus by AGENT_HUES.len() yields 0..6, fits u8"
        )]
        let slot = AgentSlot((self.len() % AGENT_HUES.len()) as u8);
        let at = self.tabs.partition_point(|t| t.id < id);
        self.tabs.insert(
            at,
            Tab {
                id,
                name,
                parent,
                recorder,
                scrollback: Scrollback::new(log_dir.join("user.log"), slot, append, self.thinking),
                retired: None,
            },
        );
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

    /// Start the linger clock at this `Died`'s position in the stream.
    pub(super) fn died(&mut self, id: AgentId) {
        let now = Instant::now();
        if let Some(tab) = self.tab_mut(id) {
            tab.retired = Some(now);
        }
        if self.focus == id {
            self.focus = self.parent_focus(id);
        }
    }

    /// Retire every descendant of `id` into the linger window — `id`'s
    /// `/clear`.  A tab already retired keeps its earlier stamp, so a child
    /// that died just before the clear is not given a fresh full window.
    /// Focus on a retired tab returns to `id`.
    pub(super) fn retire_subtree(&mut self, id: AgentId) {
        let now = Instant::now();
        let doomed: Vec<AgentId> = self
            .tabs
            .iter()
            .filter(|t| t.in_bar() && self.ancestors(t.id).any(|a| a == id))
            .map(|t| t.id)
            .collect();
        for tab in self.tabs.iter_mut().filter(|t| doomed.contains(&t.id)) {
            tab.retired.get_or_insert(now);
        }
        if doomed.contains(&self.focus) {
            self.focus = id;
        }
    }

    /// Attach to `id`. Matrix navigation selects any displayed row; `/focus`
    /// first resolves a name against the live tabs.
    pub(super) fn set_focus(&mut self, id: AgentId) {
        self.focus = id;
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

    /// Every view, tombstones included, in id order — `App::flush_logs`'s
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
        let focused = self.focused();
        self.tabs
            .iter()
            .filter(|t| t.in_bar())
            .map(|t| TabRow {
                id: t.id,
                name: &t.name,
                parent: t.parent,
                sb: &t.scrollback,
                lingering: t.lingering(),
                demoted: t.demotion(&self.fleet, focused),
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

    /// Every tab's agent id.
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
