//! Session/view lifecycle — viewports, tab order and names, the parent chain
//! focus falls back along, and the linger clock.  [`super::App`]'s `tabs` field.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::text::Line;

use crate::bus::AgentId;

use super::block::{AgentSlot, Reveal};
use super::viewport::Viewport;
use super::{LINGER, ROOT_NAME};

/// One [`Viewport`] per session, plus the tab bar that orders them.
///
/// Focus is purely presentational: `TAB` and `/focus` write it, rendering reads
/// it, and no agent-side lifecycle depends on it.  Reads route through
/// [`Self::focused`], which resolves a stale id — a tab that aged out — to root.
#[allow(clippy::struct_field_names)] // `tabs` is the natural name for the tab list.
pub(super) struct Tabs {
    /// Retained past `Died` and past tab-bar expiry so `App::flush_logs` can
    /// still write each session's `user.log`.
    viewports: HashMap<AgentId, Viewport>,
    /// Birth order, root first — stable per-session log paths across runs.
    dispatch_order: Vec<AgentId>,
    /// Root is always a member, which is what makes [`Self::focus_next`]'s walk
    /// terminate.
    tabs: Vec<AgentId>,
    names: HashMap<AgentId, String>,
    /// Death stamps for lingering subagents: the tab drops after [`LINGER`],
    /// the viewport does not.
    dying: HashMap<AgentId, Instant>,
    root: AgentId,
    focus: AgentId,
    /// Each tab's spawning agent, so focus can climb toward the trunk when the
    /// focused agent ends.  A `/branch` has no entry: it roots its own tree.
    parents: HashMap<AgentId, AgentId>,
    /// The rung thinking traces read at across every view — `/thinking`'s
    /// datum, kept here because it outlives any one viewport: a tab born after
    /// the command was typed inherits it.
    traces: Reveal,
    title_frame: u64,
}

impl Tabs {
    pub fn new(root_id: AgentId, root_log_dir: &Path, append: bool) -> Self {
        let traces = Reveal::Full;
        let mut viewports = HashMap::new();
        viewports.insert(
            root_id,
            Viewport::new(
                root_log_dir.join("user.log"),
                AgentSlot::default(),
                append,
                traces,
            ),
        );
        let mut names = HashMap::new();
        names.insert(root_id, ROOT_NAME.to_string());
        Self {
            viewports,
            dispatch_order: vec![root_id],
            tabs: vec![root_id],
            names,
            dying: HashMap::new(),
            root: root_id,
            focus: root_id,
            parents: HashMap::new(),
            traces,
            title_frame: 0,
        }
    }

    /// The focused tab, resolving a stale focus — a subagent that aged out of
    /// the bar — to root.
    pub(super) fn focused(&self) -> AgentId {
        if self.tabs.contains(&self.focus) {
            self.focus
        } else {
            self.root
        }
    }

    /// Nearest still-live ancestor tab of `id`, else root: where focus lands
    /// when the focused agent ends.
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

    /// Age `dying` entries out past [`LINGER`], once per frame.  An expired view
    /// is evicted to a tombstone rather than dropped from `viewports`: the entry
    /// must survive for the `/resources` dead-view count and `flush_logs`'s
    /// log-path listing.  Returns whether a tab went — the cue to repaint.
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
            self.names.remove(&id);
            if self.focus == id {
                self.focus = self.parent_focus(id);
            }
            self.parents.remove(&id);
            if let Some(vp) = self.viewports.get_mut(&id) {
                vp.evict_to_tombstone(id);
            }
        }
        self.title_frame += 1;
        changed
    }

    pub(super) fn born(
        &mut self,
        id: AgentId,
        log_dir: &Path,
        name: String,
        parent: Option<AgentId>,
        agent_slot: AgentSlot,
    ) {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.viewports.entry(id) {
            slot.insert(Viewport::new(
                log_dir.join("user.log"),
                agent_slot,
                false,
                self.traces,
            ));
            self.dispatch_order.push(id);
        }
        self.names.insert(id, name);
        if let Some(parent) = parent {
            self.parents.insert(id, parent);
        }
        if !self.tabs.contains(&id) {
            self.tabs.push(id);
        }
    }

    /// Flip the standing rung for thinking traces and apply it to every view at
    /// once — every trace on screen and every one still to arrive.  Reports the
    /// rung now in force, which `/thinking` names back to the user.
    pub(super) fn toggle_traces(&mut self) -> Reveal {
        self.traces = match self.traces {
            Reveal::Full => Reveal::Summary,
            _ => Reveal::Full,
        };
        for vp in self.viewports.values_mut() {
            vp.set_traces_level(self.traces);
        }
        self.traces
    }

    /// Whether `id` is a `/branch` tab — the only kind `/close` may kill.  A
    /// branch is exactly a spawned tab that roots its own tree.
    pub(super) fn is_branch(&self, id: AgentId) -> bool {
        id != self.root && !self.parents.contains_key(&id)
    }

    pub(super) fn died(&mut self, id: AgentId) {
        if id != self.root {
            self.dying.insert(id, Instant::now());
            if self.focus == id {
                self.focus = self.parent_focus(id);
            }
        }
    }

    /// Retire every non-root tab into the linger window — `/clear`.
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
        self.focus = self.root;
    }

    /// Cycle `TAB` focus past any tab in `demoted`.  Root is never demoted, so
    /// one pass around `tabs` always finds a landing spot.
    pub(super) fn focus_next(&mut self, demoted: &HashMap<AgentId, Duration>) {
        let current = self.focused();
        let pos = self.tabs.iter().position(|&id| id == current).unwrap_or(0);
        let n = self.tabs.len();
        for step in 1..=n {
            let next = self.tabs[(pos + step) % n];
            if !demoted.contains_key(&next) {
                self.focus = next;
                return;
            }
        }
    }

    /// Land focus on `id` — `/focus`, the only way to reach a demoted tab.  No
    /// validation: the caller has already checked [`Self::is_tab`].
    pub(super) fn set_focus(&mut self, id: AgentId) {
        self.focus = id;
    }

    /// Whether `id` has a tab yet.  `/focus` checks it so a name whose `Born`
    /// has not reached the frontend is refused, not silently focused.
    pub(super) fn is_tab(&self, id: AgentId) -> bool {
        self.tabs.contains(&id)
    }

    pub(super) fn viewport(&self, id: AgentId) -> Option<&Viewport> {
        self.viewports.get(&id)
    }

    pub(super) fn viewport_mut(&mut self, id: AgentId) -> Option<&mut Viewport> {
        self.viewports.get_mut(&id)
    }

    pub(super) fn root(&self) -> AgentId {
        self.root
    }

    /// Tabs in the bar — not the viewport count, which outlives them.
    pub(super) fn len(&self) -> usize {
        self.tabs.len()
    }

    pub(super) fn is_dying(&self, id: AgentId) -> bool {
        self.dying.contains_key(&id)
    }

    /// Rows for the matrix: each visible tab paired with its viewport.
    pub(super) fn matrix_rows(&self) -> Vec<(AgentId, &Viewport)> {
        self.tabs
            .iter()
            .filter_map(|&id| self.viewports.get(&id).map(|vp| (id, vp)))
            .collect()
    }

    pub(super) fn dispatch_order(&self) -> &[AgentId] {
        &self.dispatch_order
    }

    pub(super) fn names(&self) -> &HashMap<AgentId, String> {
        &self.names
    }

    pub(super) fn dying_map(&self) -> &HashMap<AgentId, Instant> {
        &self.dying
    }

    pub(super) fn parents(&self) -> &HashMap<AgentId, AgentId> {
        &self.parents
    }

    pub(super) fn viewports(&self) -> &HashMap<AgentId, Viewport> {
        &self.viewports
    }

    pub(super) fn viewports_mut(&mut self) -> &mut HashMap<AgentId, Viewport> {
        &mut self.viewports
    }

    /// The ids a viewport event can legitimately name — read only by the
    /// trace that reports a dropped one.
    #[cfg(debug_assertions)]
    pub(super) fn viewport_keys(&self) -> Vec<AgentId> {
        self.viewports.keys().copied().collect()
    }

    /// One line per tombstoned view — id, final status, log path.  Order is a
    /// `HashMap` walk, so a caller wanting stability sorts.
    pub(super) fn tombstone_lines(&self) -> Vec<Line<'static>> {
        self.viewports
            .values()
            .filter_map(|vp| vp.tombstone())
            .map(super::viewport::Tombstone::line)
            .collect()
    }

    /// Frame counter driving the terminal tab-title spinner.
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
        let mut tabs = Tabs::new(root, std::path::Path::new("/tmp/test"), false);
        tabs.parents.insert(child, root);
        tabs.parents.insert(grandchild, child);
        tabs.dying.insert(child, Instant::now());
        tabs.tabs.push(child);
        tabs.tabs.push(grandchild);
        assert_eq!(tabs.parent_focus(grandchild), root);
    }

    /// Killing one agent is never paid for out of a sibling's scrollback:
    /// tombstoning frees only the expired view.
    #[test]
    fn tick_tombstones_only_the_expired_view_leaving_a_live_sibling_untouched() {
        let root = 1;
        let child = 2;
        let mut tabs = Tabs::new(root, std::path::Path::new("/tmp/test-root"), false);
        tabs.born(
            child,
            std::path::Path::new("/tmp/test-child"),
            "child".into(),
            Some(root),
            AgentSlot(1),
        );
        if let Some(vp) = tabs.viewport_mut(child) {
            vp.push_chrome(RailShape::Plain, vec![Line::from("child says hi")]);
        }
        if let Some(vp) = tabs.viewport_mut(root) {
            vp.push_chrome(RailShape::Plain, vec![Line::from("root says hi")]);
        }
        tabs.died(child);
        // Backdate rather than wait LINGER out in a test.
        tabs.dying.insert(
            child,
            Instant::now()
                .checked_sub(LINGER + Duration::from_secs(1))
                .unwrap(),
        );

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

    /// Root is never demoted, so `TAB` wraps back to it rather than sticking on
    /// the demoted tab it skips.
    #[test]
    fn focus_next_skips_demoted_tabs() {
        let root = 1;
        let promoted = 2;
        let demoted = 3;
        let mut tabs = Tabs::new(root, std::path::Path::new("/tmp/test-focus-next"), false);
        tabs.tabs.push(promoted);
        tabs.tabs.push(demoted);

        let mut demoted_set = HashMap::new();
        demoted_set.insert(demoted, Duration::from_mins(10));

        tabs.focus_next(&demoted_set);
        assert_eq!(
            tabs.focused(),
            promoted,
            "TAB skips straight past the demoted tab"
        );

        tabs.focus_next(&demoted_set);
        assert_eq!(
            tabs.focused(),
            root,
            "TAB skips the demoted tab again, wrapping back to root"
        );
    }
}
