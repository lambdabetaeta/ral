//! The multi-agent status strip: one row per live session, drawn above the
//! transcript by [`super::render`].  The rows are a pure projection of
//! [`super::tabs`] and [`super::viewport`]; [`Matrix`] is the one retained
//! value, and it holds an agent identity, never a row number.

use super::line;
use super::palette::{AGENT_HUES, PROMPT_INK, SLATE};
use super::rail;
use super::tabs::TabRow;
use super::viewport::Viewport;
use crate::bus::AgentId;
use crate::provider;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

/// Row order for the matrix: `Spawn` leaves the spawn tree alone (roots first,
/// each one's descendants depth-first, in birth order); `Cost` keeps that same
/// tree but sorts each parent's children by cumulative token spend, heaviest
/// first.
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

/// The matrix's whole retained state: which agent the navigation cursor names,
/// or nothing while the strip is a status display.
#[derive(Clone, Copy, Debug)]
pub(super) enum Matrix {
    Watching,
    Navigating(AgentId),
}

/// A matrix gesture, as the keyboard names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Nav {
    Toggle,
    Up,
    Down,
    Attach,
    Leave,
}

/// The gesture `k` names, given whether navigation is up.  Esc is the
/// matrix's only while it is up, so a cancelled turn stays Esc's meaning
/// everywhere else.
pub(super) fn nav(k: &KeyEvent, navigating: bool) -> Option<Nav> {
    let bare = k.modifiers.is_empty();
    match k.code {
        KeyCode::Tab if bare => Some(Nav::Toggle),
        // Shift-Tab arrives as BackTab, carrying the modifier that named it.
        KeyCode::BackTab if navigating => Some(Nav::Up),
        KeyCode::Up if navigating && bare => Some(Nav::Up),
        KeyCode::Down if navigating && bare => Some(Nav::Down),
        KeyCode::Enter if navigating && bare => Some(Nav::Attach),
        KeyCode::Esc if navigating => Some(Nav::Leave),
        _ => None,
    }
}

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

/// Render the matrix into at most `height` lines.  The ordered forest is built
/// whole before the window clips it, so sibling-last information for `├─`/`└─`
/// and the ancestor bars never depends on the visible slice.  `cursor` is the
/// navigation cursor when navigation owns the keyboard; the window otherwise
/// anchors on the attached row.
pub(super) fn strip(
    rows: &[TabRow<'_>],
    focused: AgentId,
    root: AgentId,
    sort: MatrixSort,
    cursor: Option<AgentId>,
    height: usize,
) -> Vec<Line<'static>> {
    // The ramp is relative to this frame's heaviest spender, because token
    // counts dwarf `rail::value_step`'s line-count thresholds.  Root is left
    // out: its cells are blank, so counting its spend would only flatten the
    // ramp for everyone else.
    let max_tokens = rows
        .iter()
        .filter(|row| row.id != root)
        .map(token_spend)
        .max()
        .unwrap_or(0);
    // Widths are measured over every row, not the visible slice, so scrolling
    // does not reflow the columns.
    let display: Vec<MatrixRow> = forest(rows, sort)
        .iter()
        .map(|tree| {
            let row = &rows[tree.index];
            MatrixRow::new(
                row,
                focused,
                root,
                max_tokens,
                &tree.prefix,
                cursor == Some(row.id),
            )
        })
        .collect();
    let widths = MatrixWidths::measure(&display);

    let anchor = cursor.unwrap_or(focused);
    let at = display.iter().position(|row| row.id == anchor).unwrap_or(0);
    let v = view(display.len(), height, at);
    let mut lines = Vec::with_capacity(height);
    if v.above > 0 {
        lines.push(more_line('↑', v.above));
    }
    lines.extend(display[v.rows].iter().map(|row| row.render(widths)));
    if v.below > 0 {
        lines.push(more_line('↓', v.below));
    }
    lines
}

fn token_spend(row: &TabRow<'_>) -> u64 {
    let u = row.vp.usage();
    u.input + u.output
}

/// What a matrix `height` lines tall draws with the cursor at `cursor`: the
/// slice of the ordered forest it shows, and the hidden rows it has a line
/// left to announce on each side — at `height <= 2` fewer than it truly
/// hides.  Pure — the same fleet and cursor always draw the same strip, so no
/// frame's height leaks into the next.
struct View {
    rows: Range<usize>,
    above: usize,
    below: usize,
}

fn view(total: usize, height: usize, cursor: usize) -> View {
    if total == 0 || height == 0 {
        return View {
            rows: 0..0,
            above: 0,
            below: 0,
        };
    }
    if total <= height {
        return View {
            rows: 0..total,
            above: 0,
            below: 0,
        };
    }
    let (edge, mid) = (height - 1, height.saturating_sub(2));
    let (start, visible) = if cursor < edge {
        (0, edge)
    } else if cursor + edge >= total {
        (total - edge, edge)
    } else {
        // Both ends are out of reach, so both boundary lines are owed.
        let visible = mid.max(1);
        (cursor - visible / 2, visible)
    };
    let end = start + visible;
    // A boundary line is spent only once the cursor's own row is paid for: the
    // human standing on a row is worth more than the count of what it hides.
    let spare = height - visible;
    let above = if spare > 0 { start } else { 0 };
    let below = if spare > usize::from(above > 0) {
        total - end
    } else {
        0
    };
    View {
        rows: start..end,
        above,
        below,
    }
}

/// Indented by the caret column, so the marker lines up with the tree under it.
fn more_line(direction: char, count: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {direction} {count} more"),
        Style::default().fg(SLATE),
    ))
}

/// One row of the ordered forest, before the window clips it.
struct TreeRow {
    index: usize,
    prefix: String,
}

/// Depth-first walk of the forest the rows' parents describe.  A root is a row
/// with no parent, or whose parent is not itself a row.  [`MatrixSort::Cost`]
/// reorders each sibling group alone, so the tree survives the sort.
fn forest(rows: &[TabRow<'_>], sort: MatrixSort) -> Vec<TreeRow> {
    let live: HashSet<AgentId> = rows.iter().map(|row| row.id).collect();
    let mut kids: HashMap<AgentId, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match row.parent {
            Some(p) if live.contains(&p) => kids.entry(p).or_default().push(i),
            _ => roots.push(i),
        }
    }
    if sort == MatrixSort::Cost {
        roots.sort_by_key(|&i| std::cmp::Reverse(token_spend(&rows[i])));
        for group in kids.values_mut() {
            group.sort_by_key(|&i| std::cmp::Reverse(token_spend(&rows[i])));
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for &i in &roots {
        descend(i, "", String::new(), rows, &kids, &mut out);
    }
    // What no root reached is exactly the rows of a parent cycle; preserving
    // them flat is more useful than dropping a row a racing stream or
    // malformed parent metadata left unrooted.
    let reached: HashSet<usize> = out.iter().map(|row| row.index).collect();
    out.extend(
        (0..rows.len())
            .filter(|i| !reached.contains(i))
            .map(|index| TreeRow {
                index,
                prefix: String::new(),
            }),
    );
    out
}

/// Depth-first, parents before children.  `pad` is the continuation columns a
/// child inherits; a root contributes none, so its children start at the left
/// margin.
fn descend(
    i: usize,
    pad: &str,
    prefix: String,
    rows: &[TabRow<'_>],
    kids: &HashMap<AgentId, Vec<usize>>,
    out: &mut Vec<TreeRow>,
) {
    out.push(TreeRow { index: i, prefix });
    let Some(group) = kids.get(&rows[i].id) else {
        return;
    };
    for (position, &kid) in group.iter().enumerate() {
        let (branch, bar) = if position + 1 == group.len() {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        descend(
            kid,
            &format!("{pad}{bar}"),
            format!("{pad}{branch}"),
            rows,
            kids,
            out,
        );
    }
}

/// The agent one row from `cursor` in the drawn order — `cursor` itself at
/// either end.
pub(super) fn neighbour(
    rows: &[TabRow<'_>],
    sort: MatrixSort,
    cursor: AgentId,
    down: bool,
) -> AgentId {
    let order = forest(rows, sort);
    let Some(at) = order.iter().position(|row| rows[row.index].id == cursor) else {
        return cursor;
    };
    let next = if down {
        (at + 1).min(order.len() - 1)
    } else {
        at.saturating_sub(1)
    };
    rows[order[next].index].id
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
    id: AgentId,
    label: String,
    steps: String,
    tokens: String,
    bar: String,
    label_style: Style,
    hue: Color,
    token_style: Style,
    dim: bool,
    cursor: bool,
    /// Idle span if demoted, `None` if promoted: the switch [`Self::render`]
    /// right-aligns steps by.
    idle: Option<Duration>,
}

impl MatrixRow {
    fn new(
        row: &TabRow<'_>,
        focused: AgentId,
        root: AgentId,
        max_tokens: u64,
        prefix: &str,
        cursor: bool,
    ) -> Self {
        let (id, vp) = (row.id, row.vp);
        let hue = AGENT_HUES
            .get(vp.agent().0 as usize)
            .copied()
            .unwrap_or(AGENT_HUES[0]);
        let dim = row.lingering;
        let idle = row.demoted;
        let demoted = idle.is_some();

        let truncated: String = row.name.chars().take(MATRIX_LABEL_W).collect();
        let (bracketed, label_style) = if demoted {
            (format!(" {truncated} "), Style::default().fg(SLATE))
        } else {
            focus_label(&truncated, hue, id == focused, dim)
        };
        // The tree prefix is outside the brackets, so focus remains visibly
        // `[name]` and the label limit truncates only the name.
        let label = format!("{prefix}{bracketed}");

        let tokens = token_spend(row);
        let value = relative_value_step(tokens, max_tokens);
        let token_style = Style::default()
            .fg(rail::lighten(hue, value))
            .add_modifier(if dim {
                Modifier::DIM
            } else {
                Modifier::empty()
            });

        Self {
            id,
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
            cursor,
            idle,
        }
    }

    fn render(&self, widths: MatrixWidths) -> Line<'static> {
        let slate = Style::default().fg(SLATE).add_modifier(if self.dim {
            Modifier::DIM
        } else {
            Modifier::empty()
        });
        let (label_w, steps_w, tokens_w, bar_w) =
            (widths.label, widths.steps, widths.tokens, widths.bar);
        // The caret is the human's mark, so it takes the human's ink.
        let caret = Style::default().fg(PROMPT_INK).add_modifier(Modifier::BOLD);
        let steps = if self.idle.is_some() {
            Span::styled(
                format!("{:>steps_w$}", self.steps),
                Style::default().fg(SLATE),
            )
        } else {
            Span::styled(
                format!("{:<steps_w$}", self.steps),
                Style::default().fg(self.hue),
            )
        };
        Line::from(vec![
            Span::styled(if self.cursor { "›" } else { " " }, caret),
            Span::styled(format!("{:<label_w$}", self.label), self.label_style),
            Span::raw("  "),
            steps,
            Span::raw("  "),
            Span::styled(format!("{:>tokens_w$}", self.tokens), self.token_style),
            Span::raw("  "),
            Span::styled(format!("{:<bar_w$}", self.bar), slate),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::testkit::{TestAgentSpec, test_agent};
    use crate::fleet::Fleet;
    use ratatui::crossterm::event::KeyModifiers;
    use std::path::Path;
    use std::sync::{Arc, Weak};

    fn tree() -> (Arc<crate::agent::Agent>, super::super::tabs::Tabs) {
        let fleet = Fleet::new();
        let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("fresh trunk");
        let mut tabs = super::super::tabs::Tabs::new(&root, false);
        let a = root.id + 1;
        let a1 = root.id + 2;
        let a2 = root.id + 3;
        let b = root.id + 4;
        let b1 = root.id + 5;
        for (id, name, parent) in [
            (a, "a", Some(root.id)),
            (a1, "a1", Some(a)),
            (a2, "a2", Some(a)),
            (b, "b", Some(root.id)),
            (b1, "b1", Some(b)),
        ] {
            tabs.born(
                id,
                Weak::new(),
                Path::new("/tmp/exarch-matrix-test"),
                name.into(),
                parent,
                super::super::block::AgentSlot(1),
            );
        }
        (root, tabs)
    }

    #[test]
    fn prefixes_keep_full_forest_shape() {
        let (root, tabs) = tree();
        let rows = tabs.rows();
        let ordered = forest(&rows, MatrixSort::Spawn);
        let prefixes: Vec<&str> = ordered.iter().map(|row| row.prefix.as_str()).collect();
        assert_eq!(
            prefixes,
            ["", "├─ ", "│  ├─ ", "│  └─ ", "└─ ", "   └─ "],
            "connectors describe siblings and descendants, not just indentation"
        );
        assert_eq!(rows[ordered[0].index].id, root.id);
    }

    #[test]
    fn the_view_holds_the_cursor_and_fills_its_lines() {
        for total in 0..24 {
            for height in 0..12 {
                for cursor in 0..total {
                    let v = view(total, height, cursor);
                    let at = format!("view({total}, {height}, {cursor})");
                    if height > 0 {
                        assert!(v.rows.contains(&cursor), "{at} lost the cursor");
                    }
                    let lines = v.rows.len() + usize::from(v.above > 0) + usize::from(v.below > 0);
                    assert!(lines <= height, "{at} asks for {lines} of {height} lines");
                    if total > height {
                        assert_eq!(lines, height, "{at} leaves a line of the strip blank");
                    }
                    for (stated, hidden, side) in [
                        (v.above, v.rows.start, "above"),
                        (v.below, total - v.rows.end, "below"),
                    ] {
                        assert!(
                            stated == 0 || stated == hidden,
                            "{at} claims {stated} rows {side}, not {hidden}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn neighbour_walks_the_drawn_order_and_stops_at_the_ends() {
        let (root, tabs) = tree();
        let rows = tabs.rows();
        let step = |id, down| neighbour(&rows, MatrixSort::Spawn, id, down);

        assert_eq!(step(root.id, false), root.id, "the first row is the top");
        let mut id = root.id;
        for n in 1..=5 {
            id = step(id, true);
            assert_eq!(id, root.id + n, "down walks the drawn order");
        }
        assert_eq!(step(id, true), id, "the last row is the bottom");
        assert_eq!(step(id, false), root.id + 4, "up walks it back");
    }

    #[test]
    fn nav_reads_esc_only_while_the_matrix_is_up() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(nav(&key(KeyCode::Tab), false), Some(Nav::Toggle));
        assert_eq!(nav(&key(KeyCode::Tab), true), Some(Nav::Toggle));
        for (code, gesture) in [
            (KeyCode::Up, Nav::Up),
            (KeyCode::Down, Nav::Down),
            (KeyCode::BackTab, Nav::Up),
            (KeyCode::Enter, Nav::Attach),
            (KeyCode::Esc, Nav::Leave),
        ] {
            assert_eq!(nav(&key(code), true), Some(gesture));
            assert_eq!(
                nav(&key(code), false),
                None,
                "{code:?} belongs to the matrix only while it is up"
            );
        }
        assert_eq!(
            nav(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), true),
            Some(Nav::Up),
            "Shift-Tab arrives as BackTab carrying its modifier"
        );
        for k in [
            KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Down, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        ] {
            assert_eq!(
                nav(&k, true),
                None,
                "a modified {:?} is not a gesture",
                k.code
            );
        }
    }
}
