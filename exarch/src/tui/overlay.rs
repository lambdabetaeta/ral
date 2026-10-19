//! The chrome every modal overlay floats in: a plane, a margin, a double-line
//! bezel with a title and a key hint, padding, and a translucent drop shadow.
//! An overlay states only its body's size; placing and framing are done here.

use super::palette::{BANNER_GOLD, CYAN, OVERLAY_BG, SLATE};
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding};

/// Cells are about 2:1, so every horizontal measure is twice its vertical one.
const MARGIN: Margin = Margin::new(2, 1);
const PAD: Margin = Margin::new(4, 1);
const SHADOW: Margin = Margin::new(2, 1);
const CHROME_W: u16 = 2 * (MARGIN.horizontal + 1 + PAD.horizontal);
const CHROME_H: u16 = 2 * (MARGIN.vertical + 1 + PAD.vertical);

/// A shadowed cell keeps its glyph; its colours are drawn this far toward
/// [`UMBRA`]. A terminal-default colour cannot be read, so its foreground is
/// dimmed instead and its background painted [`UMBRA`] outright.
const SHADOW_ALPHA: f32 = 0.7;
const UMBRA: Color = Color::Rgb(2, 3, 8);

#[derive(Clone, Copy)]
pub(super) enum Anchor {
    Centre,
    /// Bottom-right, leaving the transcript above uncovered.
    Corner,
}

/// The body width an overlay asking for `want` columns gets inside `frame`.
pub(super) fn body_width(want: u16, frame: Rect) -> u16 {
    want.min(frame.width.saturating_sub(CHROME_W + SHADOW.horizontal))
}

/// Draw the chrome around a `w × h` body at `anchor` in `frame`, clamped to
/// fit with room for the shadow, and return the body's rect.
pub(super) fn draw(
    f: &mut Frame,
    frame: Rect,
    anchor: Anchor,
    (w, rows): (u16, usize),
    title: &str,
    hint: &str,
) -> Rect {
    let room = Rect {
        width: frame.width.saturating_sub(SHADOW.horizontal),
        height: frame.height.saturating_sub(SHADOW.vertical),
        ..frame
    };
    let h = u16::try_from(rows).unwrap_or(u16::MAX);
    let (w, h) = (
        (w + CHROME_W).min(room.width),
        h.saturating_add(CHROME_H).min(room.height),
    );
    let area = match anchor {
        Anchor::Centre => Rect {
            x: room.x + (room.width - w) / 2,
            y: room.y + (room.height - h) / 2,
            width: w,
            height: h,
        },
        Anchor::Corner => Rect {
            x: room.right() - w,
            y: room.bottom() - h,
            width: w,
            height: h,
        },
    };
    let plane = Style::default().bg(OVERLAY_BG);
    shadow(f, area);
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(plane), area);
    let bezel = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
        .style(plane)
        .title(
            Line::from(Span::styled(
                title.to_string(),
                plane.fg(BANNER_GOLD).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        )
        .title_bottom(
            Line::from(Span::styled(
                hint.to_string(),
                plane.fg(SLATE).add_modifier(Modifier::DIM),
            ))
            .centered(),
        )
        .padding(Padding::symmetric(PAD.horizontal, PAD.vertical));
    let framed = area.inner(MARGIN);
    let body = bezel.inner(framed);
    f.render_widget(bezel, framed);
    body
}

/// Darken the cells down-and-right of `area`, its top-right corner left lit:
/// the light falls from the upper left.
fn shadow(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    let bottom = Rect {
        x: area.x + SHADOW.horizontal,
        y: area.bottom(),
        width: area.width,
        height: SHADOW.vertical,
    };
    let right = Rect {
        x: area.right(),
        y: area.y + SHADOW.vertical,
        width: SHADOW.horizontal,
        height: area.height.saturating_sub(SHADOW.vertical),
    };
    for (x, y) in [bottom, right]
        .into_iter()
        .flat_map(Rect::positions)
        .map(|p| (p.x, p.y))
    {
        let Some(cell) = buf.cell_mut((x, y)) else {
            continue;
        };
        match cell.fg {
            Color::Rgb(..) => cell.fg = super::palette::mix(cell.fg, UMBRA, SHADOW_ALPHA),
            _ => cell.modifier.insert(Modifier::DIM),
        }
        cell.bg = match cell.bg {
            Color::Rgb(..) => super::palette::mix(cell.bg, UMBRA, SHADOW_ALPHA),
            _ => UMBRA,
        };
    }
}
