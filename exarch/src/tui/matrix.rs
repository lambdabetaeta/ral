use super::LINGER;
use super::line::{self, AGENT_HUES, SLATE};
use super::rail;
use super::render::tab_bar;
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

/// Columns the matrix label is truncated/padded to, so the step cells,
/// token readout, and size bar align into a grid down the rows.
pub(super) const MATRIX_LABEL_W: usize = 10;
/// Most-recent step cells a matrix row shows; a longer run keeps the tail.
pub(super) const MATRIX_STEPS_W: usize = 8;

/// The multi-agent matrix: one row per live session, columns
/// `label  steps  tokens  sizebar  Nst`.  Rows = agents in `sort` order,
/// coloured by each agent's rail hue so the matrix and the rail share one
/// identity.  A *projection* of the existing `tabs`/`viewports` model —
/// with a single session it collapses to [`tab_bar`]'s exact output, so
/// the common case is visually unchanged.
///
/// `rows` pairs each tab's id with its viewport (matrix figures are
/// derived from the viewport: step cells, lines touched, token spend);
/// `titles`/`focused`/`dying` carry the same row state `tab_bar` reads.
pub(super) fn matrix_bar(
    rows: &[(AgentId, &Viewport)],
    titles: &HashMap<AgentId, String>,
    focused: AgentId,
    dying: &HashMap<AgentId, Instant>,
    sort: MatrixSort,
) -> Vec<Line<'static>> {
    if rows.len() <= 1 {
        let tabs: Vec<AgentId> = rows.iter().map(|(id, _)| *id).collect();
        return vec![tab_bar(&tabs, titles, focused, dying)];
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
    // dwarf `rail::value_step`'s line-count thresholds.
    let max_tokens = rows
        .iter()
        .map(|(_, vp)| {
            let u = vp.usage();
            u.input + u.output
        })
        .max()
        .unwrap_or(0);
    order
        .into_iter()
        .map(|i| {
            let (id, vp) = rows[i];
            matrix_row(id, vp, titles, focused, dying, max_tokens)
        })
        .collect()
}

/// One matrix row: `label  steps  tokens  sizebar  Nst`, hued by the
/// agent's rail slot.  Focused row bold; dying rows dim and carry the
/// `LINGER` countdown in place of the size bar's right margin.
pub(super) fn matrix_row(
    id: AgentId,
    vp: &Viewport,
    titles: &HashMap<AgentId, String>,
    focused: AgentId,
    dying: &HashMap<AgentId, Instant>,
    max_tokens: u64,
) -> Line<'static> {
    let hue = AGENT_HUES
        .get(vp.agent().0 as usize)
        .copied()
        .unwrap_or(AGENT_HUES[0]);
    let dim = dying.contains_key(&id);

    // Label: truncated/padded to a fixed column, focused in brackets.
    let title = titles.get(&id).map(String::as_str).unwrap_or("?");
    let truncated: String = title.chars().take(MATRIX_LABEL_W).collect();
    let label = if id == focused {
        format!("[{truncated}]")
    } else {
        format!(" {truncated} ")
    };
    let pad = (MATRIX_LABEL_W + 2).saturating_sub(label.chars().count());
    let mut label_style = Style::default().fg(hue);
    if id == focused {
        label_style = label_style.add_modifier(Modifier::BOLD);
    }
    if dim {
        label_style = label_style.add_modifier(Modifier::DIM);
    }
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(label, label_style),
        Span::raw(" ".repeat(pad + 1)),
    ];

    // Step cells: `●` step-with-tool-call, `○` without; the most-recent
    // window so a long run never overruns the row.  A done/dying session
    // leads with `√`, an errored one with `╳`.
    spans.push(Span::styled(step_cells(vp, dim), Style::default().fg(hue)));

    // Token readout: cumulative spend, lightened toward white in
    // proportion to the heaviest spender this frame.
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
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("{:>6}", provider::humanize_tokens(tokens)),
        token_style,
    ));

    // Size readout: a `▓`-bar over lines touched (Phase 3's size-bar
    // idiom), then the step count as `Nst`.
    let touched = vp.lines_touched();
    spans.push(Span::raw("  "));
    spans.push(line::size_bar(touched));
    spans.push(Span::styled(
        format!("  {}st", vp.steps().len()),
        Style::default().fg(SLATE).add_modifier(if dim {
            Modifier::DIM
        } else {
            Modifier::empty()
        }),
    ));

    if dim && let Some(t) = dying.get(&id) {
        let left = LINGER.saturating_sub(t.elapsed()).as_secs();
        spans.push(Span::styled(
            format!(" ({left}s)"),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// The matrix row's step glyphs: `done` leads the cell run with `√`
/// (session in its linger window) or `╳` (last block an error); otherwise
/// each step renders `●` (a tool call landed within it) or `○` (none).
/// Capped to [`MATRIX_STEPS_W`] keeping the most-recent steps.
pub(super) fn step_cells(vp: &Viewport, dying: bool) -> String {
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
pub(super) fn relative_value_step(tokens: u64, max_tokens: u64) -> u8 {
    if max_tokens == 0 {
        return 0;
    }
    (tokens * 3).div_ceil(max_tokens).min(3) as u8
}
