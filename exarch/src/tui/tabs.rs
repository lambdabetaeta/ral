//! Session/view lifecycle — one [`Tab`] per agent the stream has announced,
//! in id order.  [`super::App`]'s `tabs` field.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::palette::{AGENT_HUES, AgentSlot};
use super::resources::ViewFigures;
use crate::agent::Agent;
use crate::agent::fleet::Fleet;
use crate::card::Card;
use crate::record::{AgentId, AgentState, Born, Emitter as Recorder};

use super::dial::Detail;
use super::scrollback::Scrollback;
use super::status::StateSpan;
use super::{DEMOTE_IDLE, LINGER};

/// What the agent is doing, and the cards it has pinned — the agent's, not the transcript's.
pub(super) struct Status {
    /// Total, never absent: the status line always has a state to name.
    pub(super) state: StateSpan,
    /// Kit-authored *state*: a `key → Card` register drawn as the right-hand
    /// column.  Never logged, and wiped by `/clear` so a pin is
    /// generation-bounded like the scrollback.  A `Vec`, not a map, so render
    /// order is first-seen insertion order.
    pins: Vec<(String, Card)>,
}

impl Status {
    pub(super) fn new() -> Self {
        Self {
            state: StateSpan::new(AgentState::Ready),
            pins: Vec::new(),
        }
    }

    /// Enter `state`, restarting the clock and the streamed count.  Re-entering
    /// the state already held is a no-op: a turn that re-drives the same wait
    /// must not reset the clock measuring how long that wait has run.
    pub(super) fn set_state(&mut self, state: AgentState) {
        if self.state.state != state {
            self.state = StateSpan::new(state);
        }
    }

    /// Count `chars` of arriving model text against the current state — what
    /// separates a stream that is delivering from one that has gone silent.
    pub(super) fn note_streamed(&mut self, chars: usize) {
        self.state.streamed = self.state.streamed.saturating_add(chars);
    }

    // ── pinned state (the register) ────────────────────────────────────────
    // The in-place analogue of a scrollback block: a pin writes a keyed slot,
    // touching neither screen nor log — pinned state is ambient, not scrollback.

    /// Overwrite the register slot `key`, or append it, keeping first-seen order.
    pub(super) fn set_pin(&mut self, key: String, card: Card) {
        match self.pins.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => *slot = card,
            None => self.pins.push((key, card)),
        }
    }

    pub(super) fn drop_pin(&mut self, key: &str) {
        self.pins.retain(|(k, _)| k != key);
    }

    pub(super) fn pins(&self) -> &[(String, Card)] {
        &self.pins
    }
}

/// A tab's life: in the bar and live, frozen in the bar, then out of it.
enum Life {
    Live(Scrollback),
    /// Frozen since the instant: still drawn, but no further event belongs in it.
    Lingering(Scrollback, Instant),
    /// Out of the bar: its whole transcript is in `user.log`, and only the path remains.
    Gone(PathBuf),
}

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
    status: Status,
    /// Retained past death and past bar expiry, so [`Tabs::flush_logs`] can
    /// still name this session's `user.log`.
    life: Life,
}

impl Tab {
    /// The tab `born` announces, `slot`ted among the bar's hues, its view
    /// opening under `opening`.
    fn new(id: AgentId, born: &Born, slot: usize, thinking: Detail, opening: Option<Card>) -> Self {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "modulus by AGENT_HUES.len() yields 0..6, fits u8"
        )]
        let slot = AgentSlot((slot % AGENT_HUES.len()) as u8);
        Self {
            id,
            name: born.name.clone(),
            parent: born.parent,
            status: Status::new(),
            life: Life::Live(Scrollback::new(
                born.log_dir.join("user.log"),
                slot,
                born.resumed.is_some(),
                thinking,
                born.recorder.clone(),
                opening,
            )),
        }
    }

    /// Whether the bar still shows this row.
    fn in_bar(&self) -> bool {
        !matches!(self.life, Life::Gone(_))
    }

    /// Frozen: still drawing its final frame, but no further event belongs in
    /// it.
    fn lingering(&self) -> bool {
        matches!(self.life, Life::Lingering(..))
    }

    /// In the bar and not frozen: a tab focus may land on.
    fn attended(&self) -> bool {
        matches!(self.life, Life::Live(_))
    }

    fn scrollback(&self) -> Option<&Scrollback> {
        match &self.life {
            Life::Live(sb) | Life::Lingering(sb, _) => Some(sb),
            Life::Gone(_) => None,
        }
    }

    fn scrollback_mut(&mut self) -> Option<&mut Scrollback> {
        match &mut self.life {
            Life::Live(sb) | Life::Lingering(sb, _) => Some(sb),
            Life::Gone(_) => None,
        }
    }

    /// Freeze a live tab as of `at`; a tab already frozen or gone keeps its life.
    fn retire(&mut self, at: Instant) {
        self.life = match std::mem::replace(&mut self.life, Life::Gone(PathBuf::new())) {
            Life::Live(sb) => Life::Lingering(sb, at),
            other => other,
        };
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

/// Every tab the stream has announced, in id order; never empty, since it is
/// built around the first.
///
/// Focus is purely presentational: matrix `Enter` and `/focus` write it, and
/// no agent-side lifecycle reads it. [`Self::focused`] resolves a stale id —
/// a tab that aged out — to the first tab in the bar.
#[allow(clippy::struct_field_names)] // `tabs` is the natural name for the tab list.
pub(super) struct Tabs {
    /// Id order: the one order [`Self::flush_logs`] lists paths in, and the
    /// matrix's row order, across runs.
    tabs: Vec<Tab>,
    focus: AgentId,
    /// The rung a group's deliberation reads at across every view —
    /// `/thinking`'s datum, kept here because it outlives any one scrollback: a
    /// tab born after the command was typed inherits it.
    thinking: Detail,
    /// Resolves a tab's name to its live agent.
    fleet: Arc<Fleet>,
}

impl Tabs {
    /// One tab, `first`'s, born as `born` says, and focused.
    pub fn new(first: AgentId, born: &Born, fleet: Arc<Fleet>, opening: Option<Card>) -> Self {
        // Born collapsed: deliberation reads as its grain and bulk, and
        // `/thinking` opens the text for whoever wants it.
        let thinking = Detail::Summary;
        Self {
            tabs: vec![Tab::new(first, born, 0, thinking, opening)],
            focus: first,
            thinking,
            fleet,
        }
    }

    fn tab(&self, id: AgentId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: AgentId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// The first attended tab, else the first in the bar, else the first tab:
    /// where a stale id lands.
    fn fallback(&self) -> &Tab {
        let first = |keep: fn(&Tab) -> bool| self.tabs.iter().find(|t| keep(t));
        first(Tab::attended)
            .or_else(|| first(Tab::in_bar))
            .unwrap_or(&self.tabs[0])
    }

    /// The focused tab, resolving a stale focus — a tab that aged out of the
    /// bar — to [`Self::fallback`].
    fn focused_tab(&self) -> &Tab {
        self.tab(self.focus)
            .filter(|t| t.in_bar())
            .unwrap_or_else(|| self.fallback())
    }

    pub(super) fn focused(&self) -> AgentId {
        self.focused_tab().id
    }

    /// The tab's ancestors, nearest first.
    fn ancestors(&self, id: AgentId) -> impl Iterator<Item = AgentId> {
        std::iter::successors(self.tab(id).and_then(|t| t.parent), |&p| {
            self.tab(p).and_then(|t| t.parent)
        })
    }

    pub(super) fn fleet(&self) -> &Fleet {
        &self.fleet
    }

    /// The focused tab's label, for the watch-only prompt hint.
    pub(super) fn focused_name(&self) -> &str {
        &self.focused_tab().name
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
        self.focused_tab()
            .scrollback()
            .expect("the focused tab is in the bar")
            .recorder()
    }

    /// The live tab named `name` — `/focus`'s target. `Fleet::claim` keeps
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
            .find(|&a| self.tab(a).is_some_and(Tab::attended))
            .unwrap_or_else(|| self.fallback().id)
    }

    /// Age lingering tabs out past [`LINGER`], once per frame.  An expired view
    /// goes to its log path rather than being dropped: the tab must survive for
    /// the `/resources` dead-view count and [`Self::flush_logs`]'s listing.
    /// Returns whether a tab went — the cue to repaint.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let mut expired = Vec::new();
        for tab in &mut self.tabs {
            tab.life = match std::mem::replace(&mut tab.life, Life::Gone(PathBuf::new())) {
                Life::Lingering(sb, at) if now.duration_since(at) >= LINGER => {
                    expired.push(tab.id);
                    tab.status = Status::new();
                    Life::Gone(sb.into_log())
                }
                other => other,
            };
        }
        for &id in &expired {
            if self.focus == id {
                self.focus = self.parent_focus(id);
            }
        }
        !expired.is_empty()
    }

    /// Open a tab for an agent the stream has just announced, `append`ing to
    /// its `user.log` when it is resumed.  Once per agent: a birth is sent by
    /// minting the agent's one emitter.
    pub(super) fn born(&mut self, id: AgentId, born: &Born, opening: Option<Card>) {
        let at = self.tabs.partition_point(|t| t.id < id);
        let tab = Tab::new(id, born, self.tabs.len(), self.thinking, opening);
        self.tabs.insert(at, tab);
    }

    /// Flip the standing rung for deliberation and apply it to every view at
    /// once — every group on screen and every one still to arrive.  Reports
    /// the rung now in force, which `/thinking` names back to the user.
    pub(super) fn toggle_thinking(&mut self) -> Detail {
        self.thinking = match self.thinking {
            Detail::Full => Detail::Summary,
            _ => Detail::Full,
        };
        for sb in self.tabs.iter_mut().filter_map(Tab::scrollback_mut) {
            sb.set_thinking_level(self.thinking);
        }
        self.thinking
    }

    /// Start the linger clock at this `Died`'s position in the stream.  A tab
    /// already retired keeps its earlier stamp.
    pub(super) fn died(&mut self, id: AgentId) {
        let now = Instant::now();
        if let Some(tab) = self.tab_mut(id) {
            tab.retire(now);
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
        let doomed: HashSet<AgentId> = self
            .tabs
            .iter()
            .filter(|t| t.in_bar() && self.ancestors(t.id).any(|a| a == id))
            .map(|t| t.id)
            .collect();
        for tab in self.tabs.iter_mut().filter(|t| doomed.contains(&t.id)) {
            tab.retire(now);
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
        self.tab(id)?.scrollback()
    }

    pub(super) fn scrollback_mut(&mut self, id: AgentId) -> Option<&mut Scrollback> {
        self.tab_mut(id)?.scrollback_mut()
    }

    pub(super) fn status(&self, id: AgentId) -> Option<&Status> {
        self.tab(id).filter(|t| t.in_bar()).map(|t| &t.status)
    }

    pub(super) fn status_mut(&mut self, id: AgentId) -> Option<&mut Status> {
        self.tab_mut(id)
            .filter(|t| t.in_bar())
            .map(|t| &mut t.status)
    }

    pub(super) fn focused_status(&self) -> Option<&Status> {
        self.status(self.focused())
    }

    pub(super) fn focused_scrollback(&self) -> Option<&Scrollback> {
        self.scrollback(self.focused())
    }

    /// Flush every scrollback — live or lingering — to its session's `user.log`;
    /// a gone tab's was written on its way out.  Returns the paths in id order,
    /// stable across runs.
    pub(super) fn flush_logs(&mut self) -> io::Result<Vec<PathBuf>> {
        self.tabs
            .iter_mut()
            .map(|t| match &mut t.life {
                Life::Live(sb) | Life::Lingering(sb, _) => Ok(sb.flush_log()?.to_path_buf()),
                Life::Gone(path) => Ok(path.clone()),
            })
            .collect()
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
            .filter_map(|t| {
                Some(TabRow {
                    id: t.id,
                    name: &t.name,
                    parent: t.parent,
                    sb: t.scrollback()?,
                    lingering: t.lingering(),
                    demoted: t.demotion(&self.fleet, focused),
                })
            })
            .collect()
    }

    /// The `/resources` view census.  A tab is dead whether it is still
    /// lingering in the bar or already gone.
    pub(super) fn census(&self) -> ViewFigures {
        let dead = self.tabs.iter().filter(|t| !t.attended()).count() as u64;
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
}

#[cfg(test)]
mod tests;
