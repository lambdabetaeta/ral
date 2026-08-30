//! Frame drawing: [`strips`] lays the frame out as a value, [`draw`] paints it
//! into a [`Term`].

use std::io::{self, Write};

use crossterm::{
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};

use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
};

use crate::bus::{AgentId, AgentState};

use super::App;
use super::app::Overlay;
use super::block::queued_prompt_rows;
use super::gesture::{FrameGeom, Toast};
use super::line;
use super::matrix::matrix_bar;
use super::palette::{AGENT_HUES, LIME_HOT, PINK, READ_W, SLATE};
use super::row::Row;
use super::select::highlight_range;
use super::status::rule_line;
use super::terminal::Term;
use super::viewport::{StateSpan, Viewport};

const PROMPT_PAD_H: u16 = 1;

/// Left gutter shared by the transcript, queued-prompt strip, and rule line,
/// so the rail sits off the terminal edge.
const LEFT_MARGIN: u16 = 2;
/// The register appears only with this many columns spare beside the
/// `READ_W`-capped transcript, and then takes all of them.
const REGISTER_MIN_W: u16 = 35;

/// Braille spinner for the terminal tab title; one glyph per four loop ticks.
const SPINNER: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// The frame's strips, top to bottom.  Zero-height strips are painted by
/// nothing, so a missing tab row or queue costs no branch downstream.
struct Strips {
    text: Rect,
    /// Beside the transcript, in the margin `READ_W` leaves free.
    register: Option<Rect>,
    queued: Rect,
    tabs: Rect,
    prompt: Rect,
    status: Rect,
    footer: Rect,
}

/// Lay out `area`.  `queued_h` and `tab_h` are the two strips whose height the
/// caller has already settled from this frame's rows.
fn strips(app: &App, area: Rect, queued_h: u16, tab_h: u16) -> Strips {
    let prompt_w = area.width.saturating_sub(2 + 2 * PROMPT_PAD_H);
    let prompt_h = app.prompt_state.height_hint(prompt_w, area.height);
    // Settled before the layout: the vertical split keeps full width, so
    // `area.width` stands in for the content row's width.
    let has_pins = app
        .tabs
        .viewport(app.tabs.focused())
        .is_some_and(|vp| !vp.pins().is_empty());
    let register_w = area.width.saturating_sub(LEFT_MARGIN + READ_W);
    let show_register = has_pins && register_w >= REGISTER_MIN_W;
    let [content, _breath, queued, tabs, prompt, status, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(queued_h),
        Constraint::Length(tab_h),
        Constraint::Length(prompt_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    // Capping the transcript at READ_W is what keeps the register to dead
    // margin, so it never narrows prose.
    let content = indent(content);
    let (text, register) = if show_register {
        let [text, register] =
            Layout::horizontal([Constraint::Length(READ_W), Constraint::Fill(1)]).areas(content);
        (text, Some(register))
    } else {
        (content, None)
    };
    Strips {
        text,
        register,
        queued: indent(queued),
        tabs,
        prompt,
        status: indent(status),
        footer,
    }
}

fn indent(r: Rect) -> Rect {
    Rect {
        x: r.x + LEFT_MARGIN,
        width: r.width.saturating_sub(LEFT_MARGIN),
        ..r
    }
}

/// Paint one frame into `term`.
pub(super) fn draw(app: &mut App, term: &mut Term) -> io::Result<()> {
    let area = Rect::from((Position::ORIGIN, term.size()?));
    let focused = app.tabs.focused();
    let root = app.tabs.root();
    // Both strips are owned lines, so this frame's rows are read here and
    // dropped before the transcript's own `&mut` borrow of the focused view.
    let rows = app.tabs.rows();
    // A lone root gets no tab row at all; a second tab of either kind brings
    // up the matrix, one row per tab, so a demoted agent is never invisible.
    let tab_h = if rows.len() > 1 {
        u16::try_from(rows.len()).unwrap_or(u16::MAX)
    } else {
        0
    };
    let matrix_lines = (tab_h > 0).then(|| matrix_bar(&rows, focused, root, app.matrix_sort));
    let prompt_hint = prompt_hint(root, app.is_steerable(), app.tabs.focused_name(), focused);
    let queued_lines = queued_lines(app, area);
    let s = strips(
        app,
        area,
        u16::try_from(queued_lines.len()).unwrap_or(u16::MAX),
        tab_h,
    );
    // Built before the `viewport_mut` borrow that `render_window` needs.
    let register_lines: Vec<Line<'static>> = match (app.tabs.viewport(focused), s.register) {
        (Some(vp), Some(reg)) => {
            let hue = AGENT_HUES
                .get(vp.agent().0 as usize)
                .copied()
                .unwrap_or(AGENT_HUES[0]);
            line::render_register(vp.pins(), reg.width, hue)
        }
        _ => Vec::new(),
    };
    let (mut rows, offset, scroll_pct) = match app.tabs.viewport_mut(focused) {
        Some(vp) => {
            let w = vp.render_window(s.text.width, s.text.height as usize);
            (w.lines, w.offset, w.scroll_pct)
        }
        None => (Vec::new(), 0, None),
    };
    paint_selection(app, &mut rows, offset);
    paint_hover(app, &mut rows, offset);
    // The screen flatten — one of the two seams where a margin rejoins its
    // content, the other being `user.log`.
    let lines: Vec<Line<'static>> = rows.into_iter().map(Row::into_line).collect();
    app.gesture.record_frame(FrameGeom {
        text: s.text,
        offset,
    });

    app.prompt_state.style_prompt(focused == root);
    let state = app
        .tabs
        .viewport(focused)
        .map_or_else(|| StateSpan::new(AgentState::Ready), Viewport::state);
    // Field-wise from here: the editor draws through `&mut prompt_state` while
    // the rest of the frame reads its siblings.
    let App {
        prompt_state,
        gesture,
        overlay,
        total_usage,
        last_input,
        context_window,
        status_model,
        ..
    } = &mut *app;

    // A tail-following redraw rewrites every visible cell, so without an
    // atomic swap a terminal scanning mid-write shows half a frame.  `drawn?`
    // waits until after `End`: a failed draw must not strand the terminal in
    // synchronized mode.
    execute!(io::stdout(), BeginSynchronizedUpdate)?;
    let drawn = term.draw(|f| {
        f.render_widget(Paragraph::new(lines.as_slice()), s.text);
        if let Some(reg) = s.register {
            f.render_widget(Paragraph::new(register_lines), reg);
        }
        f.render_widget(Paragraph::new(queued_lines), s.queued);
        if let Some(matrix) = matrix_lines {
            f.render_widget(Paragraph::new(matrix), s.tabs);
        }
        f.render_widget(
            Paragraph::new(rule_line(
                s.text.width.min(READ_W) as usize,
                state,
                scroll_pct,
                total_usage,
                *last_input,
                *context_window,
                status_model,
            )),
            s.status,
        );
        // One geometry for the box's interior: the editor draws inside it, and
        // the completion popup aligns its left edge with the text.
        let inner = prompt_block(Style::default()).inner(s.prompt);
        if let Some(line) = prompt_hint {
            let block = prompt_block(Style::default().fg(SLATE).add_modifier(Modifier::DIM));
            f.render_widget(Paragraph::new(line).block(block), s.prompt);
        } else {
            f.render_widget(prompt_block(Style::default().fg(PINK)), s.prompt);
            prompt_state.render(f, inner);
            // No native cursor while an overlay owns the keyboard, or it
            // peeks out from beneath it.
            if overlay.is_none()
                && let Some((x, y)) = prompt_state.cursor_screen_position()
            {
                f.set_cursor_position(Position::new(
                    inner.x + x.min(inner.width.saturating_sub(1)),
                    inner.y + y.min(inner.height.saturating_sub(1)),
                ));
            }
        }
        // The slash-command popup rises out of the prompt box over the
        // transcript, reserving no row of its own: nothing below the prompt
        // is free, and a layout that grew and shrank with the typing would
        // shift the whole frame under the reader.
        if let Some(menu) = prompt_state.menu() {
            let h = menu.height().min(s.prompt.y);
            menu.render(
                f,
                Rect {
                    x: inner.x,
                    y: s.prompt.y - h,
                    width: inner.width,
                    height: h,
                },
            );
        }
        f.render_widget(Paragraph::new(footer_hint()), s.footer);
        if let Some(toast) = gesture.toast() {
            let msg = match toast {
                Toast::Copied(n) => format!("[{n} characters copied]"),
                Toast::CopyFailed => "[copy failed]".to_owned(),
            };
            let w = u16::try_from(msg.len()).unwrap_or(u16::MAX).min(s.footer.width);
            let r = Rect {
                x: s.footer.right() - w,
                width: w,
                ..s.footer
            };
            f.render_widget(Paragraph::new(msg).style(Style::default().fg(LIME_HOT)), r);
        }
        // Last, so the overlay floats over every strip already painted.
        match overlay {
            Some(Overlay::Picker(p)) => p.render(f, area),
            Some(Overlay::Login(l)) => l.render(f, area),
            None => {}
        }
    });
    execute!(io::stdout(), EndSynchronizedUpdate)?;
    emit_tab_title(app);
    drawn?;
    Ok(())
}

/// What the human typed mid-exchange and is still waiting on — prompts and
/// the commands queued among them, in the order typed, through the same
/// chrome a committed prompt echo uses.  Capped at a third of the frame.
fn queued_lines(app: &App, area: Rect) -> Vec<Line<'static>> {
    let queued = app.inbox.queued_human_messages();
    if queued.is_empty() {
        return Vec::new();
    }
    let w = area.width.saturating_sub(LEFT_MARGIN).min(READ_W);
    queued_prompt_rows(&queued, w, usize::from((area.height / 3).max(1)))
        .into_iter()
        .map(Row::into_line)
        .collect()
}

/// Set the terminal tab title: a spinner until the root yields to the human,
/// a block once it waits.  Skipped when the composed title is unchanged.
fn emit_tab_title(app: &mut App) {
    let working = !app.inbox.waiting_for_input();
    let glyph = if working {
        SPINNER[(app.tabs.title_frame() / 4) as usize % SPINNER.len()]
    } else {
        '█'
    };
    let title = format!("{glyph} exarch: {}", app.cwd_basename);
    if app.last_title == title {
        return;
    }
    let seq = ral_core::ansi::osc_set_title(&title);
    let _ = std::io::stdout().write_all(seq.as_bytes());
    let _ = std::io::stdout().flush();
    app.last_title = title;
}

/// Reverse-video the part of the active selection inside the visible window.
fn paint_selection(app: &App, rows: &mut [Row], offset: usize) {
    let Some((lo, hi)) = app.gesture.selection() else {
        return;
    };
    for (i, row) in rows.iter_mut().enumerate() {
        let n = offset + i;
        if !(lo.row..=hi.row).contains(&n) {
            continue;
        }
        // Every row goes through `highlight_range`, interior ones included: it
        // is the only thing that knows where content starts, and an interior
        // row painted span-wise would light the margin.
        let from = if n == lo.row { lo.col } else { 0 };
        let to = if n == hi.row { hi.col } else { u16::MAX };
        highlight_range(row, from, to);
    }
}

/// Light the rail glyph of the hovered dialable block.  Only its first row
/// carries a glyph, so a block scrolled past its header shows no mark.
fn paint_hover(app: &App, rows: &mut [Row], offset: usize) {
    let Some(target) = app.gesture.hover() else {
        return;
    };
    let Some(vp) = app.tabs.focused_viewport() else {
        return;
    };
    if let Some(row) = vp
        .block_head(target)
        .and_then(|head| head.checked_sub(offset))
        .and_then(|i| rows.get_mut(i))
    {
        row.hover();
    }
}

/// The watch-only banner for a subagent tab's prompt slot, or `None` where
/// the textarea is editable.
fn prompt_hint(
    root: AgentId,
    focused_steerable: bool,
    name: &str,
    focused: AgentId,
) -> Option<Line<'static>> {
    if focused == root || focused_steerable {
        return None;
    }
    Some(Line::from(Span::styled(
        format!(" watching {name} — tab to main to steer "),
        Style::default()
            .fg(SLATE)
            .add_modifier(Modifier::DIM | Modifier::ITALIC),
    )))
}

/// The prompt's rounded chrome in `border` ink: the editor renders bare text,
/// so the box is exarch's, and the hint and editor paths share it.
fn prompt_block(border: Style) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .padding(Padding::horizontal(PROMPT_PAD_H))
}

fn footer_hint() -> Line<'static> {
    let st = Style::default()
        .fg(SLATE)
        .add_modifier(Modifier::DIM | Modifier::ITALIC);
    let hint =
        " Tab pane | drag copy (⇧ native) | Ctrl-X Ctrl-E editor | Ctrl-C cancel | /quit to leave ";
    Line::from(Span::styled(hint, st))
}
