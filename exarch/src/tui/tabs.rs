//! Session/view lifecycle management.
//!
//! Owns the viewports, tab ordering, titles, parent-child relationships,
//! focus handle, and the linger/age-out clock.  Extracted from [`super::App`]
//! (Phase 4 of the TUI modularisation).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ratatui::text::Line;

use crate::bus::AgentId;
use crate::fleet::NO_FOCUS;

use super::block::AgentSlot;
use super::viewport::Viewport;
use super::{LINGER, ROOT_TITLE};

/// Session/view lifecycle state.
///
/// Owns one [`Viewport`] per session and the tab bar that orders them.
/// The currently focused tab is resolved via the shared [`AtomicU64`]
/// handle; when it is stale (an expired tab, or the no-focus sentinel),
/// focus resolves to root.
pub(super) struct Tabs {
    /// Per-session scrollback.  Populated by `Born`, retained across
    /// `Died` and across tab-bar expiry so [`super::App::flush_logs`] can
    /// still write each session's `user.log` at session end.
    viewports: HashMap<AgentId, Viewport>,
    /// Insertion order of viewports — root first, then subagents as
    /// they were born.  Drives [`super::App::flush_logs`] for stable
    /// per-session log paths across runs.
    dispatch_order: Vec<AgentId>,
    /// Tabs visible in the tab bar.  Always starts with `root`; sub-
    /// agents are appended on `Born` and removed when their entry in
    /// `dying` ages out past [`LINGER`].
    tabs: Vec<AgentId>,
    /// Per-session label.  Root maps to [`ROOT_TITLE`]; subagents to
    /// the `title` field of their `Kind::Born` event.
    titles: HashMap<AgentId, String>,
    /// Death timestamps for subagents in their linger window.  Tabs
    /// drop from [`Self::tabs`] once [`LINGER`] elapses; the viewport
    /// stays alive for log flushing.
    dying: HashMap<AgentId, Instant>,
    root: AgentId,
    /// The fleet's focused-agent handle, shared with the agents' drive loops
    /// (an [`AtomicU64`] of an [`AgentId`], or `NO_FOCUS`).  `TAB` stores into
    /// it; the focused agent reads it in its park predicate and parks for the
    /// human.  Reads route through [`Self::focused`] so a stale id (an expired
    /// tab, or the no-focus sentinel) resolves to root.  Bound to the trunk's
    /// shared handle by [`Self::bind_focus`].
    focus: Arc<AtomicU64>,
    /// Each tab's parent (the spawning agent), recorded from `Kind::Born`, so
    /// focus can fall back to the parent — recursing toward the trunk — when a
    /// focused agent ends.
    parents: HashMap<AgentId, AgentId>,
    /// The tabs born as a `/branch` — a conversing fork of their parent, which
    /// `/close` may kill (a returning sub-agent tab may not).  Recorded from
    /// `Kind::Born`'s `branch` flag and cleaned when a tab is finally retired.
    branches: HashSet<AgentId>,
    /// Frame counter incremented each tick, driving the terminal tab-title
    /// spinner.
    title_frame: u64,

    /// Whether the currently focused tab is steerable — root always is, a live
    /// sub-agent with a registered mailbox is, and a dead/lingering one is not.
    /// Set by the UI loop on every keypress; also updated on focus-fallback so
    /// the prompt hint flips at once rather than waiting for the next keystroke.
    focused_steerable: bool,
}

impl Tabs {
    pub fn new(root_id: AgentId, root_log_dir: &Path) -> Self {
        let mut viewports = HashMap::new();
        viewports.insert(
            root_id,
            Viewport::new(root_log_dir.join("user.log"), AgentSlot::default()),
        );
        let mut titles = HashMap::new();
        titles.insert(root_id, ROOT_TITLE.to_string());
        Self {
            viewports,
            dispatch_order: vec![root_id],
            tabs: vec![root_id],
            titles,
            dying: HashMap::new(),
            root: root_id,
            // A placeholder until [`Self::bind_focus`] wires the trunk's shared
            // handle; `focused()` resolves the no-focus sentinel to root.
            focus: Arc::new(AtomicU64::new(NO_FOCUS)),
            parents: HashMap::new(),
            branches: HashSet::new(),
            title_frame: 0,
            focused_steerable: true,
        }
    }

    /// Currently focused tab.  Resolves a stale focus (a subagent that aged
    /// out of the tab bar, or the no-focus sentinel) to the root.
    pub(super) fn focused(&self) -> AgentId {
        let f = self.focus.load(Ordering::Relaxed);
        if self.tabs.contains(&f) { f } else { self.root }
    }

    /// Walk up the `parents` chain from a (dying) agent to the nearest still-
    /// live ancestor tab, falling back to root — the focus target when a
    /// focused agent ends.
    pub(super) fn parent_focus(&self, id: AgentId) -> AgentId {
        let mut cur = id;
        while let Some(&p) = self.parents.get(&cur) {
            if self.tabs.contains(&p) && !self.dying.contains_key(&p) {
                return p;
            }
            cur = p;
        }
        self.root
    }

    /// Expire `dying` entries that have outlived [`LINGER`].  Called
    /// once per frame from the event loop.  When the focused tab
    /// expires, focus falls back to its parent (recursing toward the trunk).
    /// Each expired view is evicted into a tombstone
    /// ([`Viewport::evict_to_tombstone`]) rather than dropped from
    /// [`Self::viewports`] outright: the map entry survives (so
    /// [`Self::viewports`]'s length — the `/resources` dead-view count, and
    /// [`Self::viewport_mut`]'s lookup for `flush_logs`'s final log-path
    /// listing) stays correct, while the heavy scrollback state is freed.
    /// Returns whether a tab actually aged out — the caller's signal that
    /// the tab bar and focus must repaint even absent any other change.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let expired: Vec<AgentId> = self
            .dying
            .iter()
            .filter(|&(_, &t)| now.duration_since(t) >= LINGER)
            .map(|(&id, _)| id)
            .collect();
        let changed = !expired.is_empty();
        for id in expired {
            self.dying.remove(&id);
            self.tabs.retain(|&t| t != id);
            self.titles.remove(&id);
            if self.focus.load(Ordering::Relaxed) == id {
                let fallback = self.parent_focus(id);
                self.focus.store(fallback, Ordering::Relaxed);
                self.focused_steerable = fallback == self.root;
            }
            self.parents.remove(&id);
            self.branches.remove(&id);
            if let Some(vp) = self.viewports.get_mut(&id) {
                vp.evict_to_tombstone(id);
            }
        }
        self.title_frame += 1;
        changed
    }

    /// Bind the Tabs's focus to the fleet's shared handle (the trunk's), so a
    /// `TAB` here and the focused agent's park predicate read and write one
    /// [`AtomicU64`].  Called once at REPL start, like [`super::App::bind_inbox`].
    pub fn bind_focus(&mut self, focus: Arc<AtomicU64>) {
        self.focus = focus;
    }
    /// Register a born sub-agent: create viewport, record title and parent, push tab.
    pub(super) fn born(
        &mut self,
        id: AgentId,
        log_dir: &Path,
        title: String,
        parent: AgentId,
        branch: bool,
        agent_slot: AgentSlot,
    ) {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.viewports.entry(id) {
            slot.insert(Viewport::new(log_dir.join("user.log"), agent_slot));
            self.dispatch_order.push(id);
        }
        self.titles.insert(id, title);
        self.parents.insert(id, parent);
        if branch {
            self.branches.insert(id);
        }
        if !self.tabs.contains(&id) {
            self.tabs.push(id);
        }
    }

    /// Whether `id` is a `/branch` tab — a conversing fork the `/close` command
    /// may kill, as opposed to a returning sub-agent's tab.
    pub(super) fn is_branch(&self, id: AgentId) -> bool {
        self.branches.contains(&id)
    }

    /// Mark a sub-agent as died: enter the linger window and fall back focus if needed.
    pub(super) fn died(&mut self, id: AgentId) {
        if id != self.root {
            self.dying.insert(id, Instant::now());
            if self.focus.load(Ordering::Relaxed) == id {
                self.focus.store(self.parent_focus(id), Ordering::Relaxed);
            }
        }
    }

    /// Retire every non-root tab into the linger window (used by /clear).
    pub(super) fn retire_all(&mut self) {
        let now = Instant::now();
        let retiring: Vec<AgentId> = self
            .tabs
            .iter()
            .copied()
            .filter(|&id| id != self.root)
            .collect();
        for id in retiring {
            self.dying.entry(id).or_insert(now);
        }
        self.focus.store(self.root, Ordering::Relaxed);
    }

    /// Cycle focus to the next tab (used by Tab key).
    pub(super) fn focus_next(&mut self) {
        let current = self.focused();
        let pos = self.tabs.iter().position(|&id| id == current).unwrap_or(0);
        let next = (pos + 1) % self.tabs.len();
        let next_id = self.tabs[next];
        self.focus.store(next_id, Ordering::Relaxed);
    }

    /// Immutable access to a viewport by id.
    pub(super) fn viewport(&self, id: AgentId) -> Option<&Viewport> {
        self.viewports.get(&id)
    }

    /// Mutable access to a viewport by id.
    pub(super) fn viewport_mut(&mut self, id: AgentId) -> Option<&mut Viewport> {
        self.viewports.get_mut(&id)
    }

    /// The root agent id.
    pub(super) fn root(&self) -> AgentId {
        self.root
    }

    /// Number of tabs visible in the tab bar.
    pub(super) fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Whether `id` is in the linger window.
    pub(super) fn is_dying(&self, id: AgentId) -> bool {
        self.dying.contains_key(&id)
    }

    /// Rows for the matrix/tab bar: each visible tab paired with its viewport.
    pub(super) fn matrix_rows(&self) -> Vec<(AgentId, &Viewport)> {
        self.tabs
            .iter()
            .filter_map(|&id| self.viewports.get(&id).map(|vp| (id, vp)))
            .collect()
    }

    /// Dispatch order of viewports (root first, then subagents in birth order).
    pub(super) fn dispatch_order(&self) -> &[AgentId] {
        &self.dispatch_order
    }

    /// Per-session titles.
    pub(super) fn titles(&self) -> &HashMap<AgentId, String> {
        &self.titles
    }

    /// Death timestamps for subagents in the linger window.
    pub(super) fn dying_map(&self) -> &HashMap<AgentId, Instant> {
        &self.dying
    }

    /// Immutable access to all viewports.
    pub(super) fn viewports(&self) -> &HashMap<AgentId, Viewport> {
        &self.viewports
    }

    /// Mutable access to all viewports.
    pub(super) fn viewports_mut(&mut self) -> &mut HashMap<AgentId, Viewport> {
        &mut self.viewports
    }

    /// All viewport ids.
    pub(super) fn viewport_keys(&self) -> Vec<AgentId> {
        self.viewports.keys().copied().collect()
    }

    /// One rendered line per tombstoned view — every dead sub-agent whose
    /// linger window has elapsed, evicted down to (agent id, final status,
    /// log path). Insertion order is arbitrary (a `HashMap` walk); callers
    /// wanting a stable order sort by whatever the line carries.
    pub(super) fn tombstone_lines(&self) -> Vec<Line<'static>> {
        self.viewports
            .values()
            .filter_map(|vp| vp.tombstone())
            .map(super::viewport::Tombstone::line)
            .collect()
    }

    /// Whether the focused tab is steerable.
    pub(super) fn is_steerable(&self) -> bool {
        self.focused_steerable
    }

    /// Set the steerable flag.
    pub(super) fn set_steerable(&mut self, s: bool) {
        self.focused_steerable = s;
    }

    /// Frame counter for the terminal tab-title spinner.
    pub(super) fn title_frame(&self) -> u64 {
        self.title_frame
    }
}

#[cfg(test)]
mod tests {
    use super::super::block::RailShape;
    use super::*;
    use ratatui::text::Line;
    use std::time::{Duration, Instant};

    #[test]
    fn parent_focus_skips_dying_ancestor() {
        let root = 1;
        let child = 2;
        let grandchild = 3;
        let mut tabs = Tabs::new(root, std::path::Path::new("/tmp/test"));
        tabs.parents.insert(child, root);
        tabs.parents.insert(grandchild, child);
        // Make the parent die
        tabs.dying.insert(child, Instant::now());
        tabs.tabs.push(child);
        tabs.tabs.push(grandchild);
        // Focus should skip dying parent and go to root
        assert_eq!(tabs.parent_focus(grandchild), root);
    }

    /// Tombstoning a dead view past `LINGER` never touches a different,
    /// live sibling's viewport — the lifecycle eviction (`tick`) and the
    /// per-viewport window cap are independent mechanisms, so killing one
    /// agent is never paid for out of another's retained scrollback.
    #[test]
    fn tick_tombstones_only_the_expired_view_leaving_a_live_sibling_untouched() {
        let root = 1;
        let child = 2;
        let mut tabs = Tabs::new(root, std::path::Path::new("/tmp/test-root"));
        tabs.born(
            child,
            std::path::Path::new("/tmp/test-child"),
            "child".into(),
            root,
            false,
            AgentSlot(1),
        );
        if let Some(vp) = tabs.viewport_mut(child) {
            vp.push_chrome(RailShape::Plain, vec![Line::from("child says hi")]);
        }
        if let Some(vp) = tabs.viewport_mut(root) {
            vp.push_chrome(RailShape::Plain, vec![Line::from("root says hi")]);
        }
        tabs.died(child);
        // Force the linger window to have already elapsed rather than
        // waiting LINGER (90s) out in a test.
        tabs.dying
            .insert(child, Instant::now() - LINGER - Duration::from_secs(1));

        tabs.tick();

        assert!(
            tabs.viewport(child).unwrap().tombstone().is_some(),
            "the dead child is tombstoned once past LINGER"
        );
        assert_eq!(
            tabs.viewport(child).unwrap().probe_figures().0,
            0,
            "the tombstoned child's scrollback is gone"
        );
        assert!(
            tabs.viewport(root).unwrap().tombstone().is_none(),
            "root is never tombstoned"
        );
        assert_eq!(
            tabs.viewport(root).unwrap().probe_figures().0,
            1,
            "the live root's own block survives the sibling's tombstoning untouched"
        );
    }
}
