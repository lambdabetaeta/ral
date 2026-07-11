//! Mouse gesture state and pointer tracking.
//!
//! Owns the frame geometry (updated each draw), the drag-selection state,
//! the copy-confirmation toast, and the hover/press tracking for the
//! focused viewport.  Methods that need viewport access receive it as a
//! parameter — `GestureState` is a pure data+policy bundle.

use super::render::FrameGeom;
use super::terminal::osc52_copy;
use super::viewport::Viewport;
use crate::bus::AgentId;
use ratatui::crossterm::event::MouseEvent;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long the copy-confirmation toast stays on screen once shown.
pub(super) const COPY_TOAST_TTL: Duration = Duration::from_secs(2);

/// A left-button press in progress.
pub(super) struct Press {
    /// Buffer row under the press — the selection anchor.
    pub(super) row: usize,
    /// Cell column within the text area (0 = left edge) under the press.
    pub(super) col: u16,
    /// Block under the press, cycled on a click over it that never
    /// dragged — a no-op when the block is not dialable.
    pub(super) block: Option<usize>,
    pub(super) dragged: bool,
}

/// Mouse/gesture state extracted from [`super::App`].
pub(super) struct GestureState {
    /// Geometry of the content area as of the last draw, so a
    /// mouse event arriving between frames maps to a buffer row.
    frame: Option<FrameGeom>,
    /// Active drag-selection in focused-viewport (row, col) coordinates,
    /// painted reversed and copied on release.  Each position is a buffer
    /// row and a cell-column within the text area (0 = left edge).
    selection: Option<((usize, u16), (usize, u16))>,
    /// Toast: "(N chars copied)" shown briefly on drag-copy, auto-dismissed.
    copy_toast: Option<(usize, Instant)>,
    /// In-flight left-button gesture: the row pressed, the block under
    /// it, and whether the pointer has since moved (a drag, not a click).
    press: Option<Press>,
    /// The dialable block the pointer currently rests over, if any — its
    /// rail glyph is painted brightened so the dial target is legible
    /// without hunting.  Tracked from pointer motion (any-motion mouse
    /// reporting) and cleared when the pointer leaves every dialable block.
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

    /// Save the frame geometry from the last draw.
    pub(super) fn record_frame(&mut self, frame: FrameGeom) {
        self.frame = Some(frame);
    }

    /// The dialable block under the pointer, or `None` over inert chrome, a
    /// non-dialable block, the dead margin past a line's end, or past the
    /// buffer.  The whole block claims the pointer — its entire vertical
    /// extent, not just the rail — so the dial glyph lights, the wheel
    /// dials, and the click cycles anywhere over a coalesced run, but the
    /// target hugs the rendered text: each row reaches only as far right as
    /// its own content, never into the empty margin beside a short line.
    pub(super) fn hover_block(
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

    /// The dialable block the pointer currently rests over, if any.
    pub(super) fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Update the hover target — called on every mouse event.
    pub(super) fn set_hover(&mut self, idx: Option<usize>) {
        self.hover = idx;
    }

    /// Begin a left-button gesture: drop any prior selection, anchor at the
    /// pressed row and column, and remember the block under it.  The cycle
    /// target is the current hover block, computed for this same event by
    /// [`super::App::mouse`] before dispatch, so a click in the dead margin
    /// (no hover block) clears selection rather than cycling.
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

    /// Extend the selection to the dragged-to row and column, clamped to
    /// the visible window.
    pub(super) fn drag(&mut self, me: MouseEvent) {
        let Some(frame) = self.frame else { return };
        let Some(press) = &mut self.press else { return };
        press.dragged = true;
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

    /// Finish a left-button gesture: a drag copies its selection, a bare
    /// click over a dialable block cycles it (L1↔L3); a bare click over
    /// inert content leaves the (already-cleared) selection empty.
    pub(super) fn release(&mut self, viewports: &mut HashMap<AgentId, Viewport>, focused: AgentId) {
        let Some(press) = self.press.take() else {
            return;
        };
        if press.dragged {
            if let Some((a, b)) = self.selection
                && let Some(vp) = viewports.get(&focused)
            {
                let text = vp.selection_text(a.min(b), a.max(b));
                self.copy_toast = Some((text.len(), Instant::now()));
                let _ = osc52_copy(&text);
            }
        } else if let Some(idx) = press.block {
            if let Some(vp) = viewports.get_mut(&focused) {
                vp.cycle_block(idx);
            }
            self.selection = None;
        }
    }

    /// Scroll the focused pane by `delta` rows (negative = up).
    #[allow(
        clippy::unused_self,
        reason = "one of a family of scroll gestures (`scroll` / `scroll_page`) invoked uniformly as `self.gesture.<method>(viewports, focused, delta)`; `scroll_page` reads `self.frame` and delegates to `self.scroll`, so dropping the receiver here would split the pair's call shape."
    )]
    pub(super) fn scroll(
        &self,
        viewports: &mut HashMap<AgentId, Viewport>,
        focused: AgentId,
        delta: isize,
    ) {
        if let Some(vp) = viewports.get_mut(&focused) {
            if delta < 0 {
                #[allow(clippy::cast_sign_loss, reason = "sign guarded by the enclosing branch")]
                let up = (-delta) as usize;
                vp.scroll_up(up);
            } else {
                #[allow(clippy::cast_sign_loss, reason = "sign guarded by the enclosing branch")]
                let down = delta as usize;
                vp.scroll_down(down);
            }
        }
    }

    /// Scroll one content-height per page key, falling back to a sane
    /// step before the first frame is drawn.
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
        self.scroll(viewports, focused, dir * page);
    }
    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.press = None;
    }
    /// The active drag-selection, if any, as ((`anchor_row`, `anchor_col`), (`head_row`, `head_col`))
    /// in buffer coordinates.  Rendered reversed; copied on release.
    pub(super) fn selection(&self) -> Option<((usize, u16), (usize, u16))> {
        self.selection
    }

    /// The copy-confirmation toast, if still live: (`char_count`, `born_at`).
    pub(super) fn copy_toast(&self) -> Option<&(usize, Instant)> {
        self.copy_toast.as_ref()
    }

    /// Whether the toast is still inside its display window plus `margin`.
    /// The margin must cover at least one frame interval: the toast has no
    /// event of its own to announce its expiry, so the periodic redraw that
    /// finally omits it has to land *after* [`COPY_TOAST_TTL`] elapses, and
    /// polling every `margin` guarantees that redraw is not skipped.
    pub(super) fn toast_live(&self, margin: Duration) -> bool {
        self.copy_toast
            .is_some_and(|(_, ts)| ts.elapsed() < COPY_TOAST_TTL + margin)
    }
}
