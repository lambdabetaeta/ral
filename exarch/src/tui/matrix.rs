//! The multi-agent status strip: one row per live session, drawn above the
//! transcript by [`super::render`].  Pure projection of the state
//! [`super::tabs`] and [`super::viewport`] own — nothing here is retained, and
//! every row is recomputed from that state each frame.

use super::line;
use super::palette::{AGENT_HUES, SLATE};
use super::rail;
use super::viewport::Viewport;
use crate::bus::AgentId;
use crate::provider;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Row order for the matrix: `Spawn` leaves the `tabs` order alone (root first,
/// subagents as born), `Cost` sorts on cumulative token spend.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum MatrixSort {
    #[default]
    Spawn,
    Cost,
}

/// Name characters a row label keeps; the column is then measured, not fixed here.
pub(super) const MATRIX_LABEL_W: usize = 10;
/// Step cells a row shows, the most recent kept.
pub(super) const MATRIX_STEPS_W: usize = 8;

/// A promoted row's label and ink: the focused tab bracketed and bold, the rest
/// spaced, dim while dying.
fn focus_label(name: &str, hue: Color, focused: bool, dying: bool) -> (String, Style) {
    let text = if focused {
        format!("[{name}]")
    } else {
        format!(" {name} ")
    };
    let mut style = Style::default().fg(hue);
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    if dying {
        style = style.add_modifier(Modifier::DIM);
    }
    (text, style)
}

/// One row per live session, columns `label  steps  tokens  sizebar`, each row in
/// its agent's rail hue so matrix and rail name the same agent the same colour.
///
/// `demoted` maps an idle, parked tab to its idle span; such a row renders
/// compact and sinks into a block below every promoted row whatever `sort` says,
/// so a live agent is demoted but never dropped.
pub(super) fn matrix_bar(
    rows: &[(AgentId, &Viewport)],
    names: &HashMap<AgentId, String>,
    focused: AgentId,
    root: AgentId,
    dying: &HashMap<AgentId, Instant>,
    demoted: &HashMap<AgentId, Duration>,
    sort: MatrixSort,
) -> Vec<Line<'static>> {
    // Stable, so spawn order still decides between equal spenders.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    if sort == MatrixSort::Cost {
        order.sort_by_key(|&i| {
            let u = rows[i].1.usage();
            std::cmp::Reverse(u.input + u.output)
        });
    }
    // The ramp is relative to this frame's heaviest spender, because token
    // counts dwarf `rail::value_step`'s line-count thresholds.  Root is left
    // out: its cells are blank, so counting its spend would only flatten the
    // ramp for everyone else.
    let max_tokens = rows
        .iter()
        .filter(|(id, _)| *id != root)
        .map(|(_, vp)| {
            let u = vp.usage();
            u.input + u.output
        })
        .max()
        .unwrap_or(0);
    // `partition` keeps relative order, so the sort above survives the split.
    let (mut display_rows, demoted_rows): (Vec<MatrixRow>, Vec<MatrixRow>) = order
        .into_iter()
        .map(|i| {
            let id = rows[i].0;
            MatrixRow::new(
                id,
                rows[i].1,
                names,
                focused,
                root,
                dying,
                demoted.get(&id).copied(),
                max_tokens,
            )
        })
        .partition(|row| row.idle.is_none());
    display_rows.extend(demoted_rows);
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
                label: w.label.max(row.label.chars().count()),
                steps: w.steps.max(row.steps.chars().count()),
                tokens: w.tokens.max(row.tokens.chars().count()),
                bar: w.bar.max(row.bar.chars().count()),
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
    hue: Color,
    token_style: Style,
    dim: bool,
    /// Idle span if demoted, `None` if promoted: both the key [`matrix_bar`]
    /// partitions on and the switch [`Self::render`] right-aligns steps by.
    idle: Option<Duration>,
}

impl MatrixRow {
    #[allow(
        clippy::too_many_arguments,
        reason = "one row's worth of render-time projection inputs; a parameter object would only rename this list, not shorten it"
    )]
    fn new(
        id: AgentId,
        vp: &Viewport,
        names: &HashMap<AgentId, String>,
        focused: AgentId,
        root: AgentId,
        dying: &HashMap<AgentId, Instant>,
        idle: Option<Duration>,
        max_tokens: u64,
    ) -> Self {
        let hue = AGENT_HUES
            .get(vp.agent().0 as usize)
            .copied()
            .unwrap_or(AGENT_HUES[0]);
        let dim = dying.contains_key(&id);
        let demoted = idle.is_some();

        let name = names.get(&id).map_or("?", String::as_str);
        let truncated: String = name.chars().take(MATRIX_LABEL_W).collect();
        let (label, label_style) = if demoted {
            (format!(" {truncated} "), Style::default().fg(SLATE))
        } else {
            focus_label(&truncated, hue, id == focused, dim)
        };

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
            } else if let Some(idle) = idle {
                idle_age_mark(idle)
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
            idle,
        }
    }

    fn render(self, widths: MatrixWidths) -> Line<'static> {
        let Self {
            label,
            steps,
            tokens,
            bar,
            label_style,
            hue,
            token_style,
            dim,
            idle,
        } = self;
        let slate = Style::default().fg(SLATE).add_modifier(if dim {
            Modifier::DIM
        } else {
            Modifier::empty()
        });
        let (label_w, steps_w, tokens_w, bar_w) =
            (widths.label, widths.steps, widths.tokens, widths.bar);
        let steps_span = if idle.is_some() {
            Span::styled(format!("{steps:>steps_w$}"), Style::default().fg(SLATE))
        } else {
            Span::styled(format!("{steps:<steps_w$}"), Style::default().fg(hue))
        };
        Line::from(vec![
            Span::styled(format!("{label:<label_w$}"), label_style),
            Span::raw("  "),
            steps_span,
            Span::raw("  "),
            Span::styled(format!("{tokens:>tokens_w$}"), token_style),
            Span::raw("  "),
            Span::styled(format!("{bar:<bar_w$}"), slate),
        ])
    }
}

/// A demoted row's idle age: `12m`, or `1h05m` past the hour.  Minute
/// granularity holds the cell still across redraws.
fn idle_age_mark(idle: Duration) -> String {
    let mins = idle.as_secs() / 60;
    if mins < 60 {
        format!("{mins}m")
    } else {
        format!("{}h{:02}m", mins / 60, mins % 60)
    }
}

/// The row's step glyphs, most recent [`MATRIX_STEPS_W`] kept: `●` a step that
/// made a tool call, `○` one that did not.  A `dying` row — one in its linger
/// window — leads with `√`, or `╳` if it ended on an error.
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

/// Bucket `tokens` by quartile of the frame's `max_tokens` into a `0..=3` step
/// for [`rail::lighten`]; with no spend anywhere, every row reads at base hue.
fn relative_value_step(tokens: u64, max_tokens: u64) -> u8 {
    if max_tokens == 0 {
        return 0;
    }
    (tokens * 3).div_ceil(max_tokens).min(3) as u8
}
