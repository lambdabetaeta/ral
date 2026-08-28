//! Mouse gesture state: frame geometry, drag-selection, the copy toast, and
//! the hover/press tracking for the focused viewport.  Viewports arrive as
//! method parameters rather than being owned here.

use super::render::FrameGeom;
use super::terminal::osc52_copy;
use super::viewport::Viewport;
use crate::bus::AgentId;
use ratatui::crossterm::event::MouseEvent;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(super) const COPY_TOAST_TTL: Duration = Duration::from_secs(2);

/// A left-button press in progress: the selection anchor as a buffer row and a
/// text-area column (0 = left edge), plus the block a click that never drags
/// will cycle.
pub(super) struct Press {
    pub(super) row: usize,
    pub(super) col: u16,
    pub(super) block: Option<usize>,
    pub(super) dragged: bool,
}

pub(super) struct GestureState {
    /// Geometry of the last drawn frame, so an event arriving between frames
    /// still maps to a buffer row.
    frame: Option<FrameGeom>,
    /// Anchor and head of the drag-selection, painted reversed and copied on
    /// release.
    selection: Option<((usize, u16), (usize, u16))>,
    /// Chars copied, and when the toast announcing them was born.
    copy_toast: Option<(usize, Instant)>,
    press: Option<Press>,
    /// Dialable block under the pointer; `render` lights its rail glyph.
    hover: Option<usize>,
}

impl GestureState {
    pub(super) fn new() -> Self {
        Self {
            frame: None,
            selection: None,
            copy_toast: None,
            press: None,
            hover: None,
        }
    }

    pub(super) fn record_frame(&mut self, frame: FrameGeom) {
        self.frame = Some(frame);
    }

    /// The dialable block under the pointer.  Its whole vertical extent claims
    /// the pointer, not just the rail glyph, but each row reaches only as far
    /// right as its own text — the margin beside a short line is dead.
    fn hover_block(
        &self,
        me: MouseEvent,
        viewports: &HashMap<AgentId, Viewport>,
        focused: AgentId,
    ) -> Option<usize> {
        let (row, col) = self.frame?.buffer_coords(me)?;
        let vp = viewports.get(&focused)?;
        let idx = vp.block_at(row)?;
        (vp.block_dialable(idx) && (col as usize) < vp.row_width(row)?).then_some(idx)
    }

    pub(super) fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Recompute the hover target.  `App::mouse` calls this before dispatch, so
    /// `press` and its wheel-dial sibling both read the event in hand.
    pub(super) fn update_hover(
        &mut self,
        me: MouseEvent,
        viewports: &HashMap<AgentId, Viewport>,
        focused: AgentId,
    ) {
        self.hover = self.hover_block(me, viewports, focused);
    }

    /// Begin a left-button gesture: drop any prior selection and anchor at the
    /// pressed cell.  With no hover block the click only clears.
    pub(super) fn press(&mut self, me: MouseEvent) {
        self.selection = None;
        self.press = None;
        let Some((row, col)) = self.frame.and_then(|f| f.buffer_coords(me)) else {
            return;
        };
        self.press = Some(Press {
            row,
            col,
            block: self.hover,
            dragged: false,
        });
    }

    /// Extend the selection to the pointer, clamped to the visible window; past
    /// either edge the viewport scrolls one `step` instead, so a drag held
    /// there keeps reaching further content rather than stalling at the
    /// frame's rim.
    pub(super) fn drag(
        &mut self,
        me: MouseEvent,
        viewports: &mut HashMap<AgentId, Viewport>,
        focused: AgentId,
        step: isize,
    ) {
        let Some(frame) = self.frame else { return };
        let Some(press) = &mut self.press else { return };
        press.dragged = true;
        if me.row < frame.text.y
            && let Some(vp) = viewports.get_mut(&focused)
        {
            vp.scroll_by(-step);
        } else if me.row >= frame.text.bottom()
            && let Some(vp) = viewports.get_mut(&focused)
        {
            vp.scroll_by(step);
        }
        let anchor = (press.row, press.col);
        let rel = me
            .row
            .saturating_sub(frame.text.y)
            .min(frame.text.height.saturating_sub(1));
        let cur = (
            frame.offset + rel as usize,
            me.column.saturating_sub(frame.text.x),
        );
        self.selection = Some((anchor, cur));
    }

    /// Finish a left-button gesture: a drag copies its selection to the host
    /// terminal over OSC 52, a bare click cycles its block between L1 and L3.
    pub(super) fn release(&mut self, viewports: &mut HashMap<AgentId, Viewport>, focused: AgentId) {
        let Some(press) = self.press.take() else {
            return;
        };
        if press.dragged {
            if let Some((a, b)) = self.selection
                && let Some(vp) = viewports.get(&focused)
            {
                let text = vp.selection_text(a.min(b), a.max(b));
                self.copy_toast = Some((text.chars().count(), Instant::now()));
                let _ = osc52_copy(&text);
            }
        } else if let Some(idx) = press.block {
            if let Some(vp) = viewports.get_mut(&focused) {
                vp.cycle_block(idx);
            }
            self.selection = None;
        }
    }

    /// Scroll one content-height less a row of overlap, or ten rows before the
    /// first frame has fixed a geometry.
    pub(super) fn scroll_page(
        &self,
        viewports: &mut HashMap<AgentId, Viewport>,
        focused: AgentId,
        dir: isize,
    ) {
        let page = self.frame.map_or(10, |f| {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "terminal height (u16) fits isize on this target"
            )]
            let rows = f.text.height.saturating_sub(1).max(1) as isize;
            rows
        });
        if let Some(vp) = viewports.get_mut(&focused) {
            vp.scroll_by(dir * page);
        }
    }
    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.press = None;
    }
    pub(super) fn selection(&self) -> Option<((usize, u16), (usize, u16))> {
        self.selection
    }

    pub(super) fn copy_toast(&self) -> Option<&(usize, Instant)> {
        self.copy_toast.as_ref()
    }

    /// Whether the toast is live, plus `margin` — which must span a frame
    /// interval, since only a redraw past the expiry can erase it.
    pub(super) fn toast_live(&self, margin: Duration) -> bool {
        self.copy_toast
            .is_some_and(|(_, ts)| ts.elapsed() < COPY_TOAST_TTL + margin)
    }
}
