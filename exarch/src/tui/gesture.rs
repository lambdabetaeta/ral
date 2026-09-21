//! Mouse gestures over the focused scrollback, as a transition system: reads of
//! the scrollback come in as `&Scrollback`, writes go out as an [`Effect`] for
//! `App` to apply.  Nothing here touches a scrollback mutably or the terminal.

use std::cmp::Ordering;
use std::io;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::MouseEvent;
use ratatui::layout::{Position, Rect};

use super::scrollback::{Hit, Scrollback};

pub(super) const COPY_TOAST_TTL: Duration = Duration::from_secs(2);

/// A buffer cell: a scrolled buffer row and a text-area column (0 = left
/// edge).  The derived order is row-major, so `min`/`max` sort a selection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Cell {
    pub(super) row: usize,
    pub(super) col: u16,
}

/// Where the content area sat in the last drawn frame; the only place mouse
/// coordinates become buffer cells.
#[derive(Clone, Copy)]
pub(super) struct FrameGeom {
    pub(super) text: Rect,
    /// First visible buffer row.
    pub(super) offset: usize,
}

impl FrameGeom {
    /// The cell under the pointer, or `None` outside the content area.
    fn cell(&self, me: MouseEvent) -> Option<Cell> {
        self.text
            .contains(Position::new(me.column, me.row))
            .then(|| self.clamped_cell(me))
    }

    /// The nearest visible cell to the pointer.
    fn clamped_cell(&self, me: MouseEvent) -> Cell {
        let rel = me
            .row
            .saturating_sub(self.text.y)
            .min(self.text.height.saturating_sub(1));
        Cell {
            row: self.offset + rel as usize,
            col: me.column.saturating_sub(self.text.x),
        }
    }

    /// Where the pointer sits against the content area's rows: `Less` above,
    /// `Greater` below, `Equal` within.
    fn overshoot(&self, me: MouseEvent) -> Ordering {
        if me.row < self.text.y {
            Ordering::Less
        } else if me.row >= self.text.bottom() {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }

    /// One page: the content height less a row of overlap.
    fn page(self) -> usize {
        usize::from(self.text.height.saturating_sub(1).max(1))
    }
}

/// A scrollback mutation the gesture asks for.
pub(super) enum Effect {
    /// Scroll by this many rows, negative for up.
    Scroll(isize),
    /// Cycle one part of one block a rung on.
    CycleBlock(Hit),
    /// Hand this text to the host clipboard; report back via
    /// [`GestureState::note_copy`].
    Copy(String),
}

/// The left-button gesture.  `Selected` outlives the release so the
/// selection stays painted until the next press or a clear.
#[derive(Clone, Copy)]
enum Phase {
    Idle,
    /// Pressed but not yet moved; `block` is what a bare click will cycle.
    Pressed {
        anchor: Cell,
        block: Option<Hit>,
    },
    Dragging {
        anchor: Cell,
        head: Cell,
    },
    Selected {
        anchor: Cell,
        head: Cell,
    },
}

/// The corner readout for a copy's outcome: every copy reports here, whether
/// the gesture was a drag or `/copy`, and none of them lands in the scrollback.
pub(super) enum Toast {
    /// A drag-selection, counted in characters.
    Copied(usize),
    /// `/copy`, counted in lines of the reply.
    Reply(usize),
    /// `/copy` of a reply past the clipboard limit: only its last bytes went.
    ReplyTail(usize),
    CopyFailed,
}

pub(super) struct GestureState {
    /// Geometry of the last drawn frame, so an event arriving between frames
    /// still maps to a buffer cell.
    frame: Option<FrameGeom>,
    phase: Phase,
    toast: Option<(Toast, Instant)>,
    /// Dialable part under the pointer; `render` lights its rail glyph.
    hover: Option<Hit>,
}

impl GestureState {
    pub(super) fn new() -> Self {
        Self {
            frame: None,
            phase: Phase::Idle,
            toast: None,
            hover: None,
        }
    }

    pub(super) fn record_frame(&mut self, frame: FrameGeom) {
        self.frame = Some(frame);
    }

    /// The dialable part under the pointer.  Its whole vertical extent claims
    /// the pointer, not just the rail glyph, but each row reaches only as far
    /// right as its own text — the margin beside a short line is dead.
    fn hover_block(&self, me: MouseEvent, sb: &Scrollback) -> Option<Hit> {
        let cell = self.frame?.cell(me)?;
        let hit = sb.block_at(cell.row)?;
        (sb.block_dialable(hit) && usize::from(cell.col) < sb.row_width(cell.row)?).then_some(hit)
    }

    pub(super) fn hover(&self) -> Option<Hit> {
        self.hover
    }

    /// Recompute the hover target.  `App::mouse` calls this before dispatch, so
    /// `press` and its wheel-dial sibling both read the event in hand.
    pub(super) fn update_hover(&mut self, me: MouseEvent, sb: Option<&Scrollback>) {
        self.hover = sb.and_then(|sb| self.hover_block(me, sb));
    }

    /// Begin a left-button gesture: drop any prior selection and anchor at the
    /// pressed cell.  Outside the content area the click only clears.
    pub(super) fn press(&mut self, me: MouseEvent) {
        self.phase = match self.frame.and_then(|f| f.cell(me)) {
            Some(anchor) => Phase::Pressed {
                anchor,
                block: self.hover,
            },
            None => Phase::Idle,
        };
    }

    /// Extend the selection to the pointer, clamped to the visible window; past
    /// either edge the scrollback scrolls one `step` as well, so a drag held
    /// there keeps reaching further content rather than stalling at the
    /// frame's rim.
    pub(super) fn drag(&mut self, me: MouseEvent, step: isize) -> Option<Effect> {
        let frame = self.frame?;
        let anchor = match self.phase {
            Phase::Pressed { anchor, .. } | Phase::Dragging { anchor, .. } => anchor,
            Phase::Idle | Phase::Selected { .. } => return None,
        };
        self.phase = Phase::Dragging {
            anchor,
            head: frame.clamped_cell(me),
        };
        match frame.overshoot(me) {
            Ordering::Less => Some(Effect::Scroll(-step)),
            Ordering::Greater => Some(Effect::Scroll(step)),
            Ordering::Equal => None,
        }
    }

    /// Finish a left-button gesture: a drag copies its selection, a bare click
    /// cycles its block.
    pub(super) fn release(&mut self, sb: Option<&Scrollback>) -> Option<Effect> {
        match self.phase {
            Phase::Dragging { anchor, head } => {
                self.phase = Phase::Selected { anchor, head };
                let (lo, hi) = (anchor.min(head), anchor.max(head));
                sb.map(|sb| Effect::Copy(sb.selection_text(lo, hi)))
            }
            Phase::Pressed { block, .. } => {
                self.phase = Phase::Idle;
                block.map(Effect::CycleBlock)
            }
            Phase::Idle | Phase::Selected { .. } => None,
        }
    }

    /// Scroll by one page in `dir`, or ten rows before the first frame has
    /// fixed a geometry.
    pub(super) fn scroll_page(&self, dir: isize) -> Effect {
        let page = self.frame.map_or(10, FrameGeom::page);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "a page is at most a terminal height (u16)"
        )]
        Effect::Scroll(dir * page as isize)
    }

    pub(super) fn clear_selection(&mut self) {
        self.phase = Phase::Idle;
    }

    /// The live selection as `(lo, hi)` in buffer order.
    pub(super) fn selection(&self) -> Option<(Cell, Cell)> {
        match self.phase {
            Phase::Dragging { anchor, head } | Phase::Selected { anchor, head } => {
                Some((anchor.min(head), anchor.max(head)))
            }
            Phase::Idle | Phase::Pressed { .. } => None,
        }
    }

    /// Record the outcome of an [`Effect::Copy`]: the count copied, or failure.
    pub(super) fn note_copy(&mut self, outcome: io::Result<usize>) {
        self.note(outcome.map_or(Toast::CopyFailed, Toast::Copied));
    }

    /// Post a copy outcome reached elsewhere, `/copy`'s among them.
    pub(super) fn note(&mut self, toast: Toast) {
        self.toast = Some((toast, Instant::now()));
    }

    /// The toast, while it is still to be shown.
    pub(super) fn toast(&self) -> Option<&Toast> {
        self.toast
            .as_ref()
            .filter(|(_, born)| born.elapsed() < COPY_TOAST_TTL)
            .map(|(t, _)| t)
    }

    /// Whether the toast is live, plus `margin` — which must span a frame
    /// interval, since only a redraw past the expiry can erase it.
    pub(super) fn toast_live(&self, margin: Duration) -> bool {
        self.toast
            .as_ref()
            .is_some_and(|(_, born)| born.elapsed() < COPY_TOAST_TTL + margin)
    }
}
