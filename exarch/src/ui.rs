//! Ratatui inline-viewport REPL.
//!
//! The host terminal owns its own scrollback; we live in a two-row
//! inline viewport at the bottom (`Viewport::Inline`): a horizontal
//! rule and, beneath it, a single-row input editor.  The editor is
//! always live so the user can compose the next message during a
//! turn without echo artefacts or lost keystrokes.
//!
//! All transcript content — turn header, user prompt echo, tool call
//! / result, errors, cost summary, banner, *and* the assistant's
//! streaming text — is rendered into a `Vec<Line<'static>>` and
//! pushed above the viewport via `Terminal::insert_before`, where it
//! becomes host-terminal scrollback (iTerm, tmux, …) for free.
//!
//! Streaming-markdown discipline.  Tokens accumulate in `open` until a
//! syntactic boundary is reached — either a paragraph break (`\n\n`)
//! or a soft size cap as a long-paragraph fallback.  Only at that
//! point does the completed chunk go through `tui_markdown` and into
//! scrollback, so cross-line constructs (bold/italic spans, fenced
//! code blocks, lists) render with full context instead of being cut
//! into fragments.  This is the standard streaming-markdown pattern:
//! defer until a closing token is unambiguous.  [`UiEvent::OpenBoundary`]
//! flushes whatever is left at end of turn.

use crate::api;
use ansi_to_tui::IntoText;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    crossterm::{
        event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use std::{
    borrow::Cow,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{OnceLock, mpsc::Sender},
    time::Instant,
};
use termimad::MadSkin;

const PINK:   Color = Color::Rgb(255,  20, 147);
const CYAN:   Color = Color::Rgb(  0, 240, 255);
const LIME:   Color = Color::Rgb( 57, 255,  20);
const YELLOW: Color = Color::Rgb(255, 234,   0);
const GOLD:   Color = Color::Rgb(255, 191,   0);
const PURPLE: Color = Color::Rgb(191,  64, 255);
const ORANGE: Color = Color::Rgb(255,  95,  31);
const RED:    Color = Color::Rgb(255,  50,  80);
const SLATE:  Color = Color::Rgb(140, 150, 170);

/// Inline viewport size: top row is a horizontal rule, bottom row
/// is the editor.  Everything else flows into host scrollback above.
const VIEWPORT_H: u16 = 2;
const INPUT_H: u16 = 1;
const PROMPT: &str = "▸ ";

/// Spinner glyphs and the colour cycle they step through while a
/// background task is running.  One step every [`SPIN_PERIOD_MS`].
const SPIN_GLYPHS: [char; 4] = ['-', '\\', '|', '/'];
const SPIN_COLORS: [Color; 6] = [PINK, CYAN, LIME, YELLOW, PURPLE, ORANGE];
const SPIN_PERIOD_MS: u128 = 110;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

const ART:   &str = include_str!("../data/banner.txt");
const EAGLE: &str = include_str!("../data/eagle.txt");

pub fn enter() -> io::Result<Term> {
    enable_raw_mode()?;
    let term = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions { viewport: Viewport::Inline(VIEWPORT_H) },
    )?;
    Ok(term)
}

pub fn leave(term: &mut Term) {
    let _ = term.clear();
    let _ = term.show_cursor();
    let _ = disable_raw_mode();
    println!();
}

/// Four event kinds.  Assistant text streams in via `Token`;
/// `OpenBoundary` finalises whatever has accumulated in the open
/// block and pushes it into scrollback.  `Cost` updates the running
/// dollar total displayed on the rule.  Everything else — turn
/// headers, tool calls and results, errors, the user's own prompt —
/// arrives pre-rendered as `Lines`.
pub enum UiEvent {
    Token(String),
    OpenBoundary,
    Lines(Vec<Line<'static>>),
    Cost(f64),
}

/// Send `lines` as a [`UiEvent::Lines`].  Errors (channel closed)
/// are swallowed: the worker has no recourse.
pub fn emit(tx: &Sender<UiEvent>, lines: Vec<Line<'static>>) {
    let _ = tx.send(UiEvent::Lines(lines));
}

pub struct App {
    /// In-flight assistant text since the last flush.  Paragraphs
    /// accumulate here until a `\n\n` boundary or the soft size cap
    /// triggers a markdown render; [`UiEvent::OpenBoundary`] flushes
    /// the remainder at end of turn.
    open: String,
    input: Editor,
    /// `Some(t0)` while a background task is running; the rule is
    /// then drawn with a spinner stepping by elapsed time since `t0`.
    busy_since: Option<Instant>,
    /// Cumulative session cost in dollars, refreshed by
    /// [`UiEvent::Cost`] after every assistant step and shown at the
    /// right edge of the rule.
    total_dollars: f64,
}

/// Soft cap on a single un-broken paragraph before we force-flush at
/// the latest `\n` (or, failing that, the byte boundary) so the user
/// is never staring at a frozen spinner during a runaway sentence.
const PARA_SOFT_CAP: usize = 1500;

impl Default for App { fn default() -> Self { Self::new() } }

impl App {
    pub fn new() -> Self {
        Self {
            open: String::new(),
            input: Editor::new(),
            busy_since: None,
            total_dollars: 0.0,
        }
    }

    pub fn busy_on(&mut self)  { self.busy_since = Some(Instant::now()); }
    pub fn busy_off(&mut self) { self.busy_since = None; }

    pub fn handle(&mut self, term: &mut Term, e: UiEvent) -> io::Result<()> {
        match e {
            UiEvent::Token(s) => {
                self.open.push_str(&s);
                self.flush_complete_paragraphs(term)?;
            }
            UiEvent::OpenBoundary => {
                let leftover = std::mem::take(&mut self.open);
                if !leftover.trim().is_empty() {
                    insert_lines(term, render_md_static(&leftover))?;
                }
            }
            UiEvent::Lines(ls) => insert_lines(term, ls)?,
            UiEvent::Cost(d) => self.total_dollars = d,
        }
        Ok(())
    }

    /// Flush every prefix of `open` that ends in a `\n\n` paragraph
    /// break.  If the buffer outgrows [`PARA_SOFT_CAP`] without any
    /// such break, fall back to splitting at the latest `\n` so a
    /// runaway sentence still appears progressively.  Each emitted
    /// chunk is rendered through `tui_markdown` with full context, so
    /// inline emphasis and code spans survive intact.
    fn flush_complete_paragraphs(&mut self, term: &mut Term) -> io::Result<()> {
        while let Some(idx) = self.open.find("\n\n") {
            let cut = idx + 2;
            let chunk: String = self.open.drain(..cut).collect();
            if !chunk.trim().is_empty() {
                insert_lines(term, render_md_static(&chunk))?;
            }
        }
        if self.open.len() >= PARA_SOFT_CAP {
            if let Some(idx) = self.open.rfind('\n') {
                let chunk: String = self.open.drain(..=idx).collect();
                if !chunk.trim().is_empty() {
                    insert_lines(term, render_md_static(&chunk))?;
                }
            }
        }
        Ok(())
    }

    pub fn draw(&mut self, term: &mut Term) -> io::Result<()> {
        term.draw(|f: &mut Frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(INPUT_H)])
                .split(f.area());
            f.render_widget(Paragraph::new(rule_line(chunks[0].width as usize, self.busy_since, self.total_dollars)), chunks[0]);
            let line = Line::from(vec![
                Span::styled(PROMPT, Style::default().fg(PINK).bold()),
                Span::raw(self.input.buf.as_str()),
            ]);
            f.render_widget(Paragraph::new(line), chunks[1]);
            let cx = chunks[1].x + (PROMPT.chars().count() + self.input.cursor) as u16;
            f.set_cursor_position((cx, chunks[1].y));
        })?;
        Ok(())
    }

    /// Take the input contents, clearing the editor.  Returns `None`
    /// for whitespace-only input.
    pub fn submit(&mut self) -> Option<String> {
        let s = self.input.take();
        let trimmed = s.trim();
        if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
    }

    pub fn key(&mut self, k: KeyEvent) { self.input.input(k); }

    /// Push the startup banner into scrollback.  Stays a method on
    /// `App` because it's the only line-builder that needs the live
    /// terminal width for the trailing rule; everything else has a
    /// reasonable fixed-width rendering.
    #[allow(clippy::too_many_arguments)]
    pub fn banner(
        &mut self,
        term: &mut Term,
        provider: &str,
        model: &str,
        system_size: usize,
        system_files: &[PathBuf],
        base: &str,
        extend_base: Option<&Path>,
        restrict_files: &[PathBuf],
        scratch: &Path,
    ) -> io::Result<()> {
        let mut lines: Vec<Line<'static>> = vec![Line::default()];
        for (a, e) in ART.lines().zip(EAGLE.lines()) {
            lines.push(Line::from(vec![
                styled(a.to_string(), PINK, true),
                Span::raw("  "),
                styled(e.to_string(), GOLD, true),
            ]));
        }
        lines.push(Line::from(Span::styled(
            " a delegate driving ral under a grant",
            Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::from(vec![
            slate("provider "), styled(provider.to_string(), CYAN, true),
            slate("    model "), styled(model.to_string(), LIME, true),
        ]));
        let base_color = if base == "dangerous" { RED } else { ORANGE };
        let none = || slate("none");
        let path_list = |paths: &[PathBuf]| -> Vec<Span<'static>> {
            let mut out = Vec::new();
            for (i, p) in paths.iter().enumerate() {
                if i > 0 { out.push(slate(", ")); }
                out.push(styled(p.display().to_string(), ORANGE, true));
            }
            out
        };
        let extend = match extend_base {
            None => vec![none()],
            Some(p) => vec![styled(p.display().to_string(), ORANGE, true)],
        };
        let restrict = if restrict_files.is_empty() { vec![none()] } else { path_list(restrict_files) };
        let mut row = vec![
            slate("base "), styled(base.to_string(), base_color, true),
            slate("    extend-base "),
        ];
        row.extend(extend);
        row.push(slate("    restrict "));
        row.extend(restrict);
        lines.push(Line::from(row));
        let size = format!("{:.1} kB", system_size as f64 / 1024.0);
        let mut sys = vec![
            slate("system prompt "), styled(size, LIME, true), slate(" · "),
        ];
        if system_files.is_empty() {
            sys.push(slate("default"));
        } else {
            sys.extend(path_list(system_files));
        }
        lines.push(Line::from(sys));
        lines.push(Line::from(vec![
            slate("scratch "), styled(scratch.display().to_string(), ORANGE, true),
        ]));
        lines.push(Line::from(Span::styled(
            "━".repeat(term_width(term)),
            Style::default().fg(PURPLE),
        )));
        insert_lines(term, lines)
    }
}

// ── Public line builders.  Producers (runtime/eval/main) call these
// to build a `Vec<Line<'static>>` and ship it through the channel as
// `UiEvent::Lines`.  Keeping the styling here means producers don't
// own any colour decisions — they just say *what* to render.

pub fn turn(n: usize) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(styled(format!("◆ turn {n:02}"), PURPLE, true)),
    ]
}

/// Echo of the user's submitted prompt.  Multi-line prompts get one
/// row each; the first carries the `▸` glyph, subsequent rows are
/// indented under it so the block reads as a single utterance.
pub fn user_prompt(s: &str) -> Vec<Line<'static>> {
    let pink_arrow = styled("▸ ".to_string(), PINK, true);
    let cont = Span::raw("  ");
    s.lines().enumerate().map(|(i, l)| {
        let head = if i == 0 { pink_arrow.clone() } else { cont.clone() };
        Line::from(vec![head, Span::styled(l.to_string(), Style::default().fg(SLATE))])
    }).collect()
}

pub fn tool_call(cmd: &str, audit: bool) -> Vec<Line<'static>> {
    let tag = if audit { "▶ shell+audit" } else { "▶ shell" };
    let mut iter = cmd.lines();
    let first = iter.next().unwrap_or("").to_string();
    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(tag, Style::default().fg(PINK)),
            Span::raw("  "),
            styled(first, YELLOW, true),
        ]),
    ];
    for l in iter {
        lines.push(Line::from(vec![
            Span::styled("│", Style::default().fg(PINK)),
            Span::raw("        "),
            Span::styled(l.to_string(), Style::default().fg(YELLOW)),
        ]));
    }
    lines
}

pub fn tool_stdout_line(s: &str) -> Vec<Line<'static>> { tee_lines_ansi(s, SLATE) }
pub fn tool_stderr_line(s: &str) -> Vec<Line<'static>> { tee_lines_ansi(s, ORANGE) }

pub fn tool_result(out: &str) -> Vec<Line<'static>> {
    let mut lines = vec![rail_only()];
    for l in out.lines() {
        lines.push(tool_result_line(l));
    }
    lines.push(rail_only());
    lines
}

pub fn error(msg: &str) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(vec![
            styled("✗ error ".to_string(), RED, true),
            Span::raw(msg.to_string()),
        ]),
    ]
}

pub fn dim(s: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        s.to_string(),
        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
    ))]
}

pub fn cost_summary(task: &api::Usage, total: &api::Usage) -> Vec<Line<'static>> {
    vec![cost_line(task, total)]
}

// ── Internals.

/// Reading-column cap.  The rule, the markdown body, and any other
/// transcript content all wrap at this width; on a wide terminal the
/// excess stays blank.  80 chosen as the longstanding Unix prose
/// width — comfortable for paragraph reading and consistent with the
/// banner which has always capped here.
const READ_W: u16 = 80;

/// Build the viewport's separator row.  Idle: a flat slate rule.
/// Busy: a coloured spinner glyph at the leftmost cell, glyph and
/// colour both stepping every [`SPIN_PERIOD_MS`].  When any cost has
/// accrued, the cumulative dollar total is anchored on the right.
fn rule_line(width: usize, busy_since: Option<Instant>, dollars: f64) -> Line<'static> {
    let cap = (width as u16).min(READ_W) as usize;
    let cost = (dollars > 0.0).then(|| format!(" ${dollars:.4} "));
    let cost_w = cost.as_ref().map(|s| s.chars().count()).unwrap_or(0);
    let (prefix, prefix_w): (Option<Span<'static>>, usize) = match busy_since {
        None => (None, 0),
        Some(t0) => {
            let step = (t0.elapsed().as_millis() / SPIN_PERIOD_MS) as usize;
            let glyph = SPIN_GLYPHS[step % SPIN_GLYPHS.len()];
            let color = SPIN_COLORS[step % SPIN_COLORS.len()];
            (Some(Span::styled(
                format!("{glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )), 2)
        }
    };
    let middle_w = cap.saturating_sub(prefix_w + cost_w);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(p) = prefix { spans.push(p); }
    spans.push(Span::styled("─".repeat(middle_w), Style::default().fg(SLATE)));
    if let Some(c) = cost {
        spans.push(Span::styled(c, Style::default().fg(LIME).add_modifier(Modifier::BOLD)));
    }
    Line::from(spans)
}

/// Push `lines` above the inline viewport.  We wrap at [`READ_W`]
/// columns and measure the wrapped row count via
/// `Paragraph::line_count` so long lines get the rows they need —
/// without this, content past the buffer width is silently clipped
/// because `insert_before` only allocates as many rows as we ask
/// for.  On wider terminals the right-of-`READ_W` portion stays
/// blank, giving a fixed reading column.
pub fn insert_lines(term: &mut Term, lines: Vec<Line<'static>>) -> io::Result<()> {
    if lines.is_empty() { return Ok(()); }
    let term_w = term.size().map(|s| s.width).unwrap_or(READ_W);
    let width = term_w.min(READ_W);
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let n = para.line_count(width) as u16;
    if n == 0 { return Ok(()); }
    term.insert_before(n, move |buf| {
        let area = Rect { width: width.min(buf.area.width), ..buf.area };
        para.render(area, buf);
    })?;
    Ok(())
}

fn term_width(term: &Term) -> usize {
    term.size().map(|s| s.width as usize).unwrap_or(80).min(80)
}

fn styled(content: String, color: Color, bold: bool) -> Span<'static> {
    let mut style = Style::default().fg(color);
    if bold { style = style.add_modifier(Modifier::BOLD); }
    Span::styled(content, style)
}

fn slate(content: &'static str) -> Span<'static> {
    Span::styled(content, Style::default().fg(SLATE))
}

fn rail_only() -> Line<'static> {
    Line::from(Span::styled("╎", Style::default().fg(LIME)))
}

/// Convert one tee'd output line into Ratatui rows.  ANSI escape
/// sequences are parsed by `ansi_to_tui` so colour-aware tools
/// (cargo, ripgrep, jq …) render with their own colours.  Spans the
/// parser leaves un-styled get our default `body` tint so plain
/// output still reads at a glance.  A line with no escapes yields
/// exactly one row; a line whose escapes spill across rows (rare —
/// most tools keep escape sequences within a line) yields one row
/// per parsed row.
fn tee_lines_ansi(text: &str, body: Color) -> Vec<Line<'static>> {
    let rail = Span::styled("╎ ", Style::default().fg(LIME));
    let parsed = text.into_text().unwrap_or_else(|_| text.to_string().into());
    let default_body = Style::default().fg(body).add_modifier(Modifier::DIM);
    if parsed.lines.is_empty() {
        return vec![Line::from(rail)];
    }
    parsed.lines.into_iter().map(|l| {
        let mut spans = vec![rail.clone()];
        spans.extend(l.spans.into_iter().map(|sp| {
            let style = if sp.style == Style::default() { default_body } else { sp.style };
            Span { content: Cow::Owned(sp.content.into_owned()), style }
        }));
        Line::from(spans)
    }).collect()
}

fn tool_result_line(src: &str) -> Line<'static> {
    let rail = Span::styled("╎ ", Style::default().fg(LIME));
    let make = |body_color: Color, dim: bool, bold: bool| {
        let mut style = Style::default().fg(body_color);
        if dim  { style = style.add_modifier(Modifier::DIM); }
        if bold { style = style.add_modifier(Modifier::BOLD); }
        Line::from(vec![rail.clone(), Span::styled(src.to_string(), style)])
    };
    match src {
        "STDOUT:" => make(LIME, true, false),
        "STDERR:" => make(ORANGE, true, false),
        s if s.starts_with("EXIT:") => {
            let color = if s.trim_end().ends_with('0') { LIME } else { RED };
            make(color, false, true)
        }
        "VALUE:" => make(PURPLE, true, false),
        s if s.starts_with("[+ audit tree") => make(PURPLE, false, false),
        _ => make(SLATE, true, false),
    }
}

fn cost_line(task: &api::Usage, total: &api::Usage) -> Line<'static> {
    let t = |n: u64| if n >= 10_000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() };
    let d = |x: f64| if x > 0.0 { format!("${x:.4}") } else { "—".into() };
    let s = |body: &str| Span::styled(body.to_string(), Style::default().fg(SLATE));
    let n = |body: String, color: Color| Span::styled(body, Style::default().fg(color));
    let dolb = |body: String, color: Color| Span::styled(body, Style::default().fg(color).bold());
    let cache = |u: &api::Usage, color: Color, out: &mut Vec<Span<'static>>| {
        if u.cache_creation == 0 && u.cache_read == 0 { return; }
        out.push(s(" ["));
        out.push(n(t(u.cache_creation), color));
        out.push(s(" wr/"));
        out.push(n(t(u.cache_read), color));
        out.push(s(" rd]"));
    };
    let mut spans = vec![
        s("┄ task "),
        n(t(task.input), LIME), s(" in / "), n(t(task.output), LIME), s(" out"),
    ];
    cache(task, LIME, &mut spans);
    spans.push(s(" · "));
    spans.push(dolb(d(task.dollars), LIME));
    spans.push(s("   total "));
    spans.push(n(t(total.input), PURPLE));
    spans.push(s(" in / "));
    spans.push(n(t(total.output), PURPLE));
    spans.push(s(" out"));
    cache(total, PURPLE, &mut spans);
    spans.push(s(" · "));
    spans.push(dolb(d(total.dollars), PURPLE));
    Line::from(spans)
}

/// Render a markdown chunk to ratatui rows via termimad → ANSI →
/// ansi-to-tui.  termimad gives us proper handling of tables, fenced
/// code, lists and quoted blocks (tui_markdown's predecessor lacked
/// table support); its `term_text` emits crossterm-styled ANSI which
/// `ansi_to_tui` parses straight into styled `Line`s.  A `┃` rail is
/// prepended to every row, and a trailing rail-only blank line is
/// appended so consecutive paragraphs in scrollback get visual
/// breathing room without losing the left-column continuity.
fn render_md_static(text: &str) -> Vec<Line<'static>> {
    let skin = md_skin();
    // Minus the rail (`┃ ` = 2 cols) so termimad's wrapping balances
    // tables and prose to fit inside our reading column.
    let body_w = (READ_W as usize).saturating_sub(2);
    let rendered = format!("{}", skin.text(text, Some(body_w)));
    let parsed = rendered.into_text().unwrap_or_else(|_| text.to_string().into());
    let rail = Span::styled("┃ ", Style::default().fg(CYAN));
    let blank_rail = Line::from(Span::styled("┃", Style::default().fg(CYAN)));
    let mut lines: Vec<Line<'static>> = parsed.lines.into_iter().map(|l| {
        let mut spans = vec![rail.clone()];
        spans.extend(l.spans.into_iter().map(|s| Span {
            content: Cow::Owned(s.content.into_owned()),
            style: s.style,
        }));
        Line::from(spans)
    }).collect();
    while lines.last().is_some_and(|l| l.spans.iter().skip(1).all(|s| s.content.trim().is_empty())) {
        lines.pop();
    }
    if !lines.is_empty() {
        lines.push(blank_rail);
    }
    lines
}

/// Cached default-dark `MadSkin`.  Built once per process — `MadSkin`
/// only carries colour/glyph configuration, so a shared reference is
/// safe to reuse across every paragraph render.
fn md_skin() -> &'static MadSkin {
    static SKIN: OnceLock<MadSkin> = OnceLock::new();
    SKIN.get_or_init(MadSkin::default_dark)
}

/// Single-line input editor: char-indexed buffer, basic motion +
/// editing.  Cursor is rendered by `Frame::set_cursor_position` so
/// the editor itself produces only spans, no inverted-cell trick.
pub struct Editor {
    buf: String,
    cursor: usize,
}

impl Default for Editor { fn default() -> Self { Self::new() } }

impl Editor {
    pub fn new() -> Self { Self { buf: String::new(), cursor: 0 } }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn input(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press { return; }
        let plain = !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match k.code {
            KeyCode::Char(c) if plain => {
                let p = self.byte_at(self.cursor);
                self.buf.insert(p, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let p = self.byte_at(self.cursor - 1);
                self.buf.remove(p);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < self.char_len() => {
                let p = self.byte_at(self.cursor);
                self.buf.remove(p);
            }
            KeyCode::Left  => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.char_len()),
            KeyCode::Home  => self.cursor = 0,
            KeyCode::End   => self.cursor = self.char_len(),
            _ => {}
        }
    }

    fn char_len(&self) -> usize { self.buf.chars().count() }

    fn byte_at(&self, c: usize) -> usize {
        self.buf.char_indices().nth(c).map(|(b, _)| b).unwrap_or(self.buf.len())
    }
}
