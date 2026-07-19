use super::line;
use super::palette::{AGENT_HUES, CYAN, SLATE};
use super::rail;
use super::viewport::Viewport;
use crate::bus::AgentId;
use crate::provider;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::time::Instant;

/// How the agent×step matrix orders its rows — a render-time projection
/// of the same `tabs`/`viewports` model, never a reshuffle of the
/// underlying state.  [`MatrixSort::Spawn`] is the default (the `tabs`
/// order, root first then subagents as born); [`MatrixSort::Cost`]
/// surfaces the budget-burner by sorting on cumulative token spend.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum MatrixSort {
    #[default]
    Spawn,
    Cost,
}

/// Max name characters a matrix row's label keeps; a longer name is
/// truncated to this. Column alignment down the rows comes from the
/// measured [`MatrixWidths`], not this cap.
pub(super) const MATRIX_LABEL_W: usize = 10;
/// Most-recent step cells a matrix row shows; a longer run keeps the tail.
pub(super) const MATRIX_STEPS_W: usize = 8;

/// One-row tab bar.  Focused tab in bold + cyan, live subagents in
/// slate, dying subagents in slate dim until they age out.  Shown only
/// when there is more than one tab — root-only sessions skip the row
/// entirely.
pub(super) fn tab_bar(
    tabs: &[AgentId],
    names: &HashMap<AgentId, String>,
    focused: AgentId,
    dying: &HashMap<AgentId, Instant>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 2);
    for (i, &id) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let name = names.get(&id).map_or("?", String::as_str);
        let label: String = if id == focused {
            format!("[{name}]")
        } else {
            format!(" {name} ")
        };
        let style = if id == focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else if dying.contains_key(&id) {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// The multi-agent matrix: one row per live session, columns
/// `label  steps  tokens  sizebar`.  Rows = agents in `sort` order,
/// coloured by each agent's rail hue so the matrix and the rail share one
/// identity.  A *projection* of the existing `tabs`/`viewports` model —
/// with a single session it collapses to [`tab_bar`]'s exact output, so
/// the common case is visually unchanged.
///
/// `rows` pairs each tab's id with its viewport (matrix figures are
/// derived from the viewport: step cells, lines touched, token spend);
/// `names`/`focused`/`dying` carry the same row state `tab_bar` reads.
pub(super) fn matrix_bar(
    rows: &[(AgentId, &Viewport)],
    names: &HashMap<AgentId, String>,
    focused: AgentId,
    root: AgentId,
    dying: &HashMap<AgentId, Instant>,
    sort: MatrixSort,
) -> Vec<Line<'static>> {
    if rows.len() <= 1 {
        let tabs: Vec<AgentId> = rows.iter().map(|(id, _)| *id).collect();
        return vec![tab_bar(&tabs, names, focused, dying)];
    }
    // Render-time row order: spawn keeps `rows` (the `tabs` order); cost
    // sorts by cumulative spend, descending, so the budget-burner floats
    // to the top.  A stable sort preserves spawn order among ties.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    if sort == MatrixSort::Cost {
        order.sort_by_key(|&i| {
            let u = rows[i].1.usage();
            std::cmp::Reverse(u.input + u.output)
        });
    }
    // The value ramp is relative to the heaviest spender this frame: the
    // top row(s) read near-white, the rest step down, so "which child
    // burned the budget" is pre-attentive even though raw token counts
    // dwarf `rail::value_step`'s line-count thresholds.  Root is excluded —
    // its magnitude is never displayed (its token/step/bar cells are blank),
    // so folding it in would leave the ramp short whenever root spends most.
    let max_tokens = rows
        .iter()
        .filter(|(id, _)| *id != root)
        .map(|(_, vp)| {
            let u = vp.usage();
            u.input + u.output
        })
        .max()
        .unwrap_or(0);
    let display_rows: Vec<MatrixRow> = order
        .into_iter()
        .map(|i| {
            MatrixRow::new(
                rows[i].0, rows[i].1, names, focused, root, dying, max_tokens,
            )
        })
        .collect();
    let widths = MatrixWidths::measure(&display_rows);
    display_rows
        .into_iter()
        .map(|row| row.render(widths))
        .collect()
}

#[derive(Clone, Copy)]
struct MatrixWidths {
    label: usize,
    steps: usize,
    tokens: usize,
    bar: usize,
}

impl MatrixWidths {
    fn measure(rows: &[MatrixRow]) -> Self {
        rows.iter().fold(
            Self {
                label: 0,
                steps: 0,
                tokens: 0,
                bar: 0,
            },
            |w, row| Self {
                label: w.label.max(width(&row.label)),
                steps: w.steps.max(width(&row.steps)),
                tokens: w.tokens.max(width(&row.tokens)),
                bar: w.bar.max(width(&row.bar)),
            },
        )
    }
}

struct MatrixRow {
    label: String,
    steps: String,
    tokens: String,
    bar: String,
    label_style: Style,
    hue: ratatui::style::Color,
    token_style: Style,
    dim: bool,
}

impl MatrixRow {
    fn new(
        id: AgentId,
        vp: &Viewport,
        names: &HashMap<AgentId, String>,
        focused: AgentId,
        root: AgentId,
        dying: &HashMap<AgentId, Instant>,
        max_tokens: u64,
    ) -> Self {
        let hue = AGENT_HUES
            .get(vp.agent().0 as usize)
            .copied()
            .unwrap_or(AGENT_HUES[0]);
        let dim = dying.contains_key(&id);

        let name = names.get(&id).map_or("?", String::as_str);
        let truncated: String = name.chars().take(MATRIX_LABEL_W).collect();
        let label = if id == focused {
            format!("[{truncated}]")
        } else {
            format!(" {truncated} ")
        };
        let mut label_style = Style::default().fg(hue);
        if id == focused {
            label_style = label_style.add_modifier(Modifier::BOLD);
        }
        if dim {
            label_style = label_style.add_modifier(Modifier::DIM);
        }

        let tokens = {
            let u = vp.usage();
            u.input + u.output
        };
        let value = relative_value_step(tokens, max_tokens);
        let token_style = Style::default()
            .fg(rail::lighten(hue, value))
            .add_modifier(if dim {
                Modifier::DIM
            } else {
                Modifier::empty()
            });

        Self {
            label,
            steps: if id == root {
                String::new()
            } else {
                step_cells(vp, dim)
            },
            tokens: if id == root {
                String::new()
            } else {
                provider::humanize_tokens(tokens)
            },
            bar: if id == root {
                String::new()
            } else {
                line::size_bar_text(vp.lines_touched())
            },
            label_style,
            hue,
            token_style,
            dim,
        }
    }

    fn render(self, widths: MatrixWidths) -> Line<'static> {
        let slate = Style::default().fg(SLATE).add_modifier(if self.dim {
            Modifier::DIM
        } else {
            Modifier::empty()
        });
        Line::from(vec![
            Span::styled(pad_right(&self.label, widths.label), self.label_style),
            Span::raw("  "),
            Span::styled(
                pad_right(&self.steps, widths.steps),
                Style::default().fg(self.hue),
            ),
            Span::raw("  "),
            Span::styled(pad_left(&self.tokens, widths.tokens), self.token_style),
            Span::raw("  "),
            Span::styled(pad_right(&self.bar, widths.bar), slate),
        ])
    }
}

/// The matrix row's step glyphs: `done` leads the cell run with `√`
/// (session in its linger window) or `╳` (last block an error); otherwise
/// each step renders `●` (a tool call landed within it) or `○` (none).
/// Capped to [`MATRIX_STEPS_W`] keeping the most-recent steps.
fn step_cells(vp: &Viewport, dying: bool) -> String {
    let steps = vp.steps();
    let tail = steps.len().saturating_sub(MATRIX_STEPS_W);
    let mut s = String::new();
    if dying {
        s.push(if vp.last_is_error() { '╳' } else { '√' });
    }
    let room = MATRIX_STEPS_W.saturating_sub(s.chars().count());
    for &had_call in steps[tail..].iter().rev().take(room).rev() {
        s.push(if had_call { '●' } else { '○' });
    }
    s
}

/// Bucket `tokens` against the frame's `max_tokens` into a `0..=3`
/// value step for [`rail::lighten`]: the heaviest spender reads brightest,
/// the rest step down by quartile of the maximum.  `max_tokens == 0`
/// (no spend yet) reads flat at the base hue.
fn relative_value_step(tokens: u64, max_tokens: u64) -> u8 {
    if max_tokens == 0 {
        return 0;
    }
    (tokens * 3).div_ceil(max_tokens).min(3) as u8
}

fn width(s: &str) -> usize {
    s.chars().count()
}

fn pad_left(s: &str, cols: usize) -> String {
    format!("{}{}", " ".repeat(cols.saturating_sub(width(s))), s)
}

fn pad_right(s: &str, cols: usize) -> String {
    let pad = cols.saturating_sub(width(s));
    format!("{s}{}", " ".repeat(pad))
}
