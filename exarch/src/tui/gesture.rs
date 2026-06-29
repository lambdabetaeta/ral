//! Mouse gesture state and pointer tracking.
//!
//! Owns the frame geometry (updated each draw), the drag-selection state,
//! the copy-confirmation toast, and the hover/press tracking for the
//! focused viewport.  Methods that need viewport access receive it as a
//! parameter — GestureState is a pure data+policy bundle.

use std::collections::HashMap;
use std::time::Instant;
use ratatui::crossterm::event::MouseEvent;
use crate::bus::AgentId;
use super::terminal::osc52_copy;
use super::render::FrameGeom;
use super::render::contains;
use super::viewport::Viewport;

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
        Self { frame: None, selection: None, copy_toast: None, press: None, hover: None }
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
        let frame = self.frame?;
        if !contains(frame.text, me.column, me.row) {
            return None;
        }
        let row = frame.offset + (me.row - frame.text.y) as usize;
        let vp = viewports.get(&focused)?;
        let idx = vp.block_at(row)?;
        let col = (me.column - frame.text.x) as usize;
        (vp.block_dialable(idx) && col < vp.row_width(row)?).then_some(idx)
}

    /// The dialable block the pointer currently rests over, if any.
    pub(super) fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Update the hover target — called on every mouse event.
    pub(super) fn set_hover(&mut self, idx: Option<usize>) {
        self.hover = idx;
    }


    /// Begin a left-button gesture: drop any prior selection, anchor at
    /// the pressed row and column, and remember the block under it.
    pub(super) fn press(
        &mut self,
        me: MouseEvent,
        viewports: &HashMap<AgentId, Viewport>,
        focused: AgentId,
    ) {
        self.selection = None;
        self.press = None;
        let Some(frame) = self.frame else { return };
        if !contains(frame.text, me.column, me.row) {
            return;
        }
        let row = frame.offset + (me.row - frame.text.y) as usize;
        let col = me.column.saturating_sub(frame.text.x);
        // The cycle target hugs the text exactly as the hover and wheel do,
        // so a click in the dead margin clears selection rather than cycling.
        let block = self.hover_block(me, viewports, focused);
        self.press = Some(Press {
            row,
            col,
            block,
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
    /// inert content stays selection (a no-op clear).
    pub(super) fn release(
        &mut self,
        viewports: &mut HashMap<AgentId, Viewport>,
        focused: AgentId,
    ) {
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
    pub(super) fn scroll(
        &mut self,
        viewports: &mut HashMap<AgentId, Viewport>,
        focused: AgentId,
        delta: isize,
    ) {
        if let Some(vp) = viewports.get_mut(&focused) {
            if delta < 0 {
                vp.scroll_up((-delta) as usize);
            } else {
                vp.scroll_down(delta as usize);
            }
        }
    }

    /// Scroll one content-height per page key, falling back to a sane
    /// step before the first frame is drawn.
    pub(super) fn scroll_page(
        &mut self,
        viewports: &mut HashMap<AgentId, Viewport>,
        focused: AgentId,
        dir: isize,
    ) {
        let page = self
            .frame
            .map(|f| f.text.height.saturating_sub(1).max(1) as isize)
            .unwrap_or(10);
        self.scroll(viewports, focused, dir * page);
    }
  pub(super) fn clear_selection(&mut self) {
      self.selection = None;
      self.press = None;
  }
  /// The active drag-selection, if any, as ((anchor_row, anchor_col), (head_row, head_col))
  /// in buffer coordinates.  Rendered reversed; copied on release.
  pub(super) fn selection(&self) -> Option<((usize, u16), (usize, u16))> {
    self.selection
  }

  /// The copy-confirmation toast, if still live: (char_count, born_at).
  pub(super) fn copy_toast(&self) -> Option<&(usize, Instant)> {
    self.copy_toast.as_ref()
  }
}
