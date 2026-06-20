//! Paragraph-at-a-time Markdown rendering.
//!
//! Walks `pulldown-cmark` events directly into `ratatui::text::Line`s, with
//! syntect-highlighted fenced code (from the bat-curated `two-face` syntax
//! set).  The viewport commits one fence-safe paragraph at a time, so each
//! call sees a structurally complete slice and no structural-repair pass is
//! needed.

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SynColor, FontStyle, Style as SynStyle, Theme};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::fidelity::Fidelity;
use super::line::{CYAN, LIME, READ_W, SLATE, is_blank};
use super::rail::mix;

/// Left inset for assistant markdown lines, marking the model's voice
/// against the column-0 chrome.
pub(super) const MD_INDENT: u16 = 4;

const CODE_BG: Color = Color::Rgb(28, 32, 42);

/// The foreground assumed for spans with no explicit colour (plain prose
/// rendered at the terminal default), so the fidelity contrast pull and
/// echo waver have a concrete value to interpolate. A degraded answer's
/// prose must visibly lose contrast, so it cannot stay at the bare
/// terminal default once modulation kicks in.
const BASE_FG: Color = Color::Rgb(208, 213, 224);

// ── public entry points ──────────────────────────────────────────────────

/// Render `text` to ratatui lines, clamped to [`READ_W`] columns.  `indent`
/// shrinks the wrap budget and prepends that many spaces to every non-blank
/// emitted line, so the prose sits inset from the surrounding chrome.
/// `fidelity` degrades the rendering medium with its source (Move 7): a
/// context-stressed turn dims and loses contrast, an echoed paragraph
/// wavers.
pub(super) fn render_md(text: &str, w: u16, indent: u16, fidelity: Fidelity) -> Vec<Line<'static>> {
    let body_w = w.min(READ_W).saturating_sub(indent).max(1) as usize;
    let mut comp = Composer::new(body_w, indent as usize);
    let mut p = Parser::new_ext(text, gfm());
    while let Some(ev) = p.next() {
        comp.event(ev, &mut p);
    }
    let mut lines = comp.finish(ends_with_blank_line(text));
    modulate(&mut lines, fidelity);
    lines
}

/// Degrade finished lines so the medium tracks the model's reliability
/// (Move 7, coherent degradation).  Both treatments walk the already-built
/// spans — fixed-colour code, headings, and tables are *not* exempt, so the
/// whole block reads as one fidelity rather than a mix of degraded prose
/// and confident code:
///
/// - **context pressure → value reduction.** Level 1 dims; levels 2–3 dim
///   and pull every foreground toward the background (~40% / ~70%), so a
///   stressed answer reads as muted.
/// - **echo similarity → waver.** Alternate rows oscillate ±1 value-step in
///   lightness, so an echoed paragraph reads as unsteady without parsing.
fn modulate(lines: &mut [Line<'static>], f: Fidelity) {
    if f.context == 0 && f.echo == 0 {
        return;
    }
    let pull = match f.context {
        0 | 1 => 0.0,
        2 => 0.40,
        _ => 0.70,
    };
    for (row, line) in lines.iter_mut().enumerate() {
        // Even rows lift toward white, odd rows toward black — a ±1
        // value-step oscillation when the paragraph echoes its script.
        let waver = (f.echo > 0).then_some(if row % 2 == 0 {
            Color::Rgb(255, 255, 255)
        } else {
            Color::Rgb(0, 0, 0)
        });
        for span in &mut line.spans {
            if span.content.trim().is_empty() {
                continue;
            }
            if f.context >= 1 {
                span.style = span.style.add_modifier(Modifier::DIM);
            }
            if pull == 0.0 && waver.is_none() {
                continue;
            }
            let mut fg = span.style.fg.unwrap_or(BASE_FG);
            if pull > 0.0 {
                fg = mix(fg, CODE_BG, pull);
            }
            if let Some(to) = waver {
                fg = mix(fg, to, 1.0 / 3.0);
            }
            span.style.fg = Some(fg);
        }
    }
}

fn gfm() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}

/// True when the text ends with a blank line — i.e. the input already
/// signals "paragraph break ahead" and the renderer should leave one
/// trailing blank as a separator for the next commit.
fn ends_with_blank_line(s: &str) -> bool {
    s.ends_with("\n\n") || s == "\n"
}

// ── composer ─────────────────────────────────────────────────────────────

/// Walks pulldown-cmark events into a `Vec<Line<'static>>`, managing wrap
/// width, inline style stack, list nesting (with per-item first-line
/// markers), and a blockquote rail.  Code fences and tables are
/// sub-rendered and stitched in via [`Self::append_rendered`] so they
/// share the same left-margin machinery as prose lines.
struct Composer {
    out: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    cur_w: usize,
    body_w: usize,
    /// Outer chrome indent applied as the leftmost layer of every
    /// non-blank emitted line.
    indent: usize,
    style: Style,
    style_stack: Vec<Style>,
    /// Active list containers, innermost last.  Total pad column is the
    /// sum of their reserved marker widths; each owns its current item's
    /// first-line marker span.
    list_stack: Vec<ListCtx>,
    /// Blockquote nesting depth — each level paints one `│ ` slate rail.
    rail_depth: usize,
    /// Open inline links — pop on `TagEnd::Link` to maybe emit `(url)`.
    links: Vec<(LinkType, String)>,
    /// True when the last pushed token abuts the next with no intervening
    /// whitespace — the on-screen word is still open.  Suppresses the
    /// pre-append soft wrap so punctuation fused to a styled span breaks
    /// with it, not at the seam.  Cleared by whitespace and line breaks.
    mid_word: bool,
}

struct ListCtx {
    /// `None` for unordered; `Some(n)` is the next ordered marker.
    next: Option<u64>,
    /// Reserved column width for the marker (alignment for continuation).
    marker_w: usize,
    /// Bullet/number span for the current item's first emitted line;
    /// consumed by `left_margin` and re-set on each `Tag::Item`.
    pending: Option<Span<'static>>,
}

impl Composer {
    fn new(body_w: usize, indent: usize) -> Self {
        Self {
            out: Vec::new(),
            cur: Vec::new(),
            cur_w: 0,
            body_w,
            indent,
            style: Style::default(),
            style_stack: Vec::new(),
            list_stack: Vec::new(),
            rail_depth: 0,
            links: Vec::new(),
            mid_word: false,
        }
    }

    /// Total list-pad column (sum of active marker widths).
    fn pad(&self) -> usize {
        self.list_stack.iter().map(|c| c.marker_w).sum()
    }

    /// Wrap budget for inline content after subtracting rail + pad.
    fn budget(&self) -> usize {
        self.body_w
            .saturating_sub(self.rail_depth * 2 + self.pad())
            .max(1)
    }

    fn event<'a, I: Iterator<Item = Event<'a>>>(&mut self, ev: Event<'a>, p: &mut I) {
        match ev {
            Event::Start(t) => self.start(t, p),
            Event::End(e) => self.end(e),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self.push_span(Span::styled(
                t.into_string(),
                Style::default().fg(LIME).bg(CODE_BG),
            )),
            Event::Html(t) | Event::InlineHtml(t) => self.text(&t),
            Event::SoftBreak => self.push_space(),
            Event::HardBreak => self.flush_line(),
            Event::Rule => self.rule(),
            Event::FootnoteReference(name) => self.push_span(Span::styled(
                format!("[^{name}]"),
                Style::default().fg(SLATE),
            )),
            Event::TaskListMarker(checked) => {
                let (text, color) = if checked {
                    ("[x] ", LIME)
                } else {
                    ("[ ] ", SLATE)
                };
                self.push_span(Span::styled(text.to_string(), Style::default().fg(color)));
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t),
        }
    }

    fn start<'a, I: Iterator<Item = Event<'a>>>(&mut self, tag: Tag<'a>, p: &mut I) {
        match tag {
            Tag::Paragraph
            | Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => self.block_break(),

            Tag::Heading { level, .. } => {
                self.block_break();
                self.push_style(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.block_break();
                self.rail_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.block_break();
                let lang = match kind {
                    CodeBlockKind::Fenced(s) => {
                        let s = s.trim();
                        (!s.is_empty()).then(|| s.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                let mut body = String::new();
                for ev in p.by_ref() {
                    match ev {
                        Event::Text(t) => body.push_str(&t),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                let lines = highlight_block(&body, lang.as_deref());
                self.append_rendered(lines);
                self.block_break();
            }
            Tag::List(first) => {
                self.block_break();
                let marker_w = match first {
                    None => 2,
                    Some(n) => n.to_string().len() + 2,
                };
                self.list_stack.push(ListCtx {
                    next: first,
                    marker_w,
                    pending: None,
                });
            }
            Tag::Item => {
                let ctx = self.list_stack.last_mut().expect("Item without List");
                let text = match ctx.next.as_mut() {
                    None => "• ".to_string(),
                    Some(n) => {
                        let s = format!("{n}. ");
                        *n += 1;
                        s
                    }
                };
                ctx.pending = Some(Span::styled(text, Style::default().fg(CYAN)));
            }
            Tag::FootnoteDefinition(name) => {
                self.block_break();
                self.push_span(Span::styled(
                    format!("[^{name}]: "),
                    Style::default().fg(SLATE),
                ));
            }
            Tag::Table(aligns) => {
                self.block_break();
                let lines = render_table(p, aligns, self.budget());
                self.append_rendered(lines);
                self.blank_separator();
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}

            // Inline tags: push style, and for `Link` also stash the URL
            // so `TagEnd::Link` can emit `(url)` after the link text.
            Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript
            | Tag::Link { .. }
            | Tag::Image { .. } => {
                self.push_style(style_delta(&tag).expect("inline tag has style"));
                if let Tag::Link {
                    link_type,
                    dest_url,
                    ..
                } = tag
                {
                    self.links.push((link_type, dest_url.into_string()));
                }
            }
        }
    }

    fn end(&mut self, e: TagEnd) {
        match e {
            TagEnd::Paragraph
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::FootnoteDefinition => self.block_break(),

            TagEnd::Heading(_) => {
                self.flush_line();
                self.pop_style();
                self.blank_separator();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.rail_depth = self.rail_depth.saturating_sub(1);
                self.blank_separator();
            }
            TagEnd::CodeBlock => {}
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                self.blank_separator();
            }
            // A tight list (pulldown-cmark emits its item text bare, with
            // no enclosing `Paragraph`) renders single-spaced: just flush
            // the item's last row, no inter-item blank.  A loose list wraps
            // each item's content in `Paragraph` events, whose `block_break`
            // already inserts the inter-item blank, so loose spacing is
            // preserved without any work here.
            TagEnd::Item => self.flush_line(),

            TagEnd::Link => {
                self.pop_style();
                if let Some((lt, url)) = self.links.pop()
                    && matches!(lt, LinkType::Inline)
                    && !url.is_empty()
                {
                    self.push_span(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
                    ));
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Image => self.pop_style(),

            TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {}
        }
    }

    // ── inline ──

    fn text(&mut self, t: &str) {
        for part in t.split_inclusive(char::is_whitespace) {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            if !trimmed.is_empty() {
                self.push_word(trimmed);
            }
            if part.len() != trimmed.len() {
                self.push_space();
            }
        }
    }

    /// Pre-append soft wrap: flush the current row when `w` columns won't
    /// fit — UNLESS we are mid-word, i.e. the token abuts the previous one
    /// with no intervening whitespace.  A glued unit (a styled span and the
    /// punctuation fused to it, like `` `code`. `` or `**bold**.`) must
    /// break together, never at its seam, so its trailing punctuation never
    /// detaches onto its own visual line.
    fn wrap_before(&mut self, w: usize) {
        if !self.mid_word && self.cur_w + w > self.budget() && !self.cur.is_empty() {
            self.flush_line();
        }
    }

    fn push_word(&mut self, word: &str) {
        let w = UnicodeWidthStr::width(word);
        let budget = self.budget();
        self.wrap_before(w);
        if w <= budget {
            self.cur.push(Span::styled(word.to_string(), self.style));
            self.cur_w += w;
            self.mid_word = true;
            return;
        }
        // Word exceeds the budget on its own; break by char.
        let mut buf = String::new();
        let mut bw = 0;
        for ch in word.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if bw + cw > budget && !buf.is_empty() {
                self.cur
                    .push(Span::styled(std::mem::take(&mut buf), self.style));
                self.cur_w += bw;
                self.flush_line();
                bw = 0;
            }
            buf.push(ch);
            bw += cw;
        }
        if !buf.is_empty() {
            self.cur.push(Span::styled(buf, self.style));
            self.cur_w += bw;
        }
        self.mid_word = true;
    }

    fn push_space(&mut self) {
        self.mid_word = false;
        if self.cur.is_empty() || self.cur_w >= self.budget() {
            return;
        }
        self.cur.push(Span::styled(" ".to_string(), self.style));
        self.cur_w += 1;
    }

    fn push_span(&mut self, span: Span<'static>) {
        let w = UnicodeWidthStr::width(span.content.as_ref());
        self.wrap_before(w);
        self.cur.push(span);
        self.cur_w += w;
        self.mid_word = true;
    }

    fn rule(&mut self) {
        self.block_break();
        let w = self.budget();
        self.cur
            .push(Span::styled("─".repeat(w), Style::default().fg(SLATE)));
        self.cur_w = w;
        self.block_break();
    }

    // ── style stack ──

    fn push_style(&mut self, d: Style) {
        self.style_stack.push(self.style);
        self.style = self.style.patch(d);
    }
    fn pop_style(&mut self) {
        self.style = self.style_stack.pop().unwrap_or_default();
    }

    // ── line management ──

    /// Flush any in-progress line then emit a blank separator unless one
    /// is already at the tail.  Called at every block boundary, opening
    /// or closing.
    fn block_break(&mut self) {
        self.flush_line();
        self.blank_separator();
    }

    fn flush_line(&mut self) {
        self.mid_word = false;
        if self.cur.is_empty() {
            return;
        }
        let mut spans = self.left_margin();
        spans.append(&mut self.cur);
        self.out.push(Line::from(spans));
        self.cur_w = 0;
    }

    /// Build the left margin for the next emitted line: outer chrome
    /// indent, then one `│ ` per rail, then `pad - innermost.marker_w`
    /// spaces and the innermost list's one-shot pending marker
    /// (consumed) — else just `pad` spaces.
    fn left_margin(&mut self) -> Vec<Span<'static>> {
        let mut sp: Vec<Span<'static>> = Vec::with_capacity(self.rail_depth + 3);
        if self.indent > 0 {
            sp.push(Span::raw(" ".repeat(self.indent)));
        }
        for _ in 0..self.rail_depth {
            sp.push(Span::styled("│ ", Style::default().fg(SLATE)));
        }
        let (marker, marker_w) = match self.list_stack.last_mut() {
            Some(c) => (c.pending.take(), c.marker_w),
            None => (None, 0),
        };
        let used = if marker.is_some() { marker_w } else { 0 };
        let pad_spaces = self.pad().saturating_sub(used);
        if pad_spaces > 0 {
            sp.push(Span::raw(" ".repeat(pad_spaces)));
        }
        if let Some(m) = marker {
            sp.push(m);
        }
        sp
    }

    fn blank_separator(&mut self) {
        if self.out.last().is_some_and(|l| !is_blank(l)) {
            self.out.push(Line::default());
        }
    }

    /// Stitch externally-rendered lines (code highlight, table) into
    /// `out`, applying the rail + pad margin to each non-blank row.
    fn append_rendered(&mut self, lines: Vec<Line<'static>>) {
        for l in lines {
            if is_blank(&l) {
                self.out.push(Line::default());
                continue;
            }
            let mut spans = self.left_margin();
            spans.extend(l.spans);
            self.out.push(Line::from(spans));
        }
    }

    /// Drain any in-progress line, strip trailing blanks, optionally
    /// append one trailing blank for paragraph separation, and yield
    /// the line buffer.
    fn finish(mut self, trailing_blank: bool) -> Vec<Line<'static>> {
        self.flush_line();
        while self.out.last().is_some_and(is_blank) {
            self.out.pop();
        }
        if trailing_blank && !self.out.is_empty() {
            self.out.push(Line::default());
        }
        self.out
    }
}

/// Inline-tag → style modifier.  Returns `None` for block and structural
/// tags.  Single source of truth shared by [`Composer::start`] and
/// [`render_table`] so the two walkers can't drift.
fn style_delta(tag: &Tag<'_>) -> Option<Style> {
    Some(match tag {
        Tag::Emphasis => Style::default().add_modifier(Modifier::ITALIC),
        Tag::Strong => Style::default().add_modifier(Modifier::BOLD),
        Tag::Strikethrough => Style::default().add_modifier(Modifier::CROSSED_OUT),
        Tag::Superscript | Tag::Subscript => Style::default().add_modifier(Modifier::DIM),
        Tag::Link { .. } => Style::default().fg(CYAN).add_modifier(Modifier::UNDERLINED),
        Tag::Image { .. } => Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
        _ => return None,
    })
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        HeadingLevel::H2 => Style::default().fg(LIME).add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H4 => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        HeadingLevel::H5 => Style::default().add_modifier(Modifier::ITALIC),
        HeadingLevel::H6 => Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
    }
}

// ── tables ───────────────────────────────────────────────────────────────

/// Consume events from `p` up to and including `End(Table)` and emit a
/// boxed table.  Column widths are the natural max, scaled down
/// proportionally if the total exceeds the budget.  Cell content beyond
/// its column width is clipped with `…`.
fn render_table<'a, I: Iterator<Item = Event<'a>>>(
    p: &mut I,
    aligns: Vec<Alignment>,
    budget: usize,
) -> Vec<Line<'static>> {
    let mut head_cells: Vec<Vec<Span<'static>>> = Vec::new();
    let mut body_rows: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    let mut cur_row: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur_cell: Vec<Span<'static>> = Vec::new();
    let mut in_head = false;
    let mut style = Style::default();
    let mut stack: Vec<Style> = Vec::new();

    for ev in p.by_ref() {
        match ev {
            Event::Start(t) => {
                if let Some(d) = style_delta(&t) {
                    stack.push(style);
                    style = style.patch(d);
                    continue;
                }
                match t {
                    Tag::TableHead => in_head = true,
                    Tag::TableRow => cur_row.clear(),
                    Tag::TableCell => cur_cell.clear(),
                    _ => {}
                }
            }
            Event::End(e) => match e {
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Superscript
                | TagEnd::Subscript
                | TagEnd::Link
                | TagEnd::Image => style = stack.pop().unwrap_or_default(),
                TagEnd::TableHead => in_head = false,
                TagEnd::TableRow => body_rows.push(std::mem::take(&mut cur_row)),
                TagEnd::TableCell => {
                    let cell = std::mem::take(&mut cur_cell);
                    if in_head {
                        head_cells.push(cell);
                    } else {
                        cur_row.push(cell);
                    }
                }
                TagEnd::Table => break,
                _ => {}
            },
            Event::Text(t) => cur_cell.push(Span::styled(t.into_string(), style)),
            Event::Code(t) => cur_cell.push(Span::styled(
                t.into_string(),
                Style::default().fg(LIME).bg(CODE_BG),
            )),
            Event::SoftBreak | Event::HardBreak => cur_cell.push(Span::raw(" ".to_string())),
            _ => {}
        }
    }

    let n_cols = head_cells
        .len()
        .max(body_rows.iter().map(Vec::len).max().unwrap_or(0));
    if n_cols == 0 {
        return Vec::new();
    }

    let nat_w = |row: &[Vec<Span<'static>>], i: usize| -> usize {
        row.get(i)
            .map(|c| {
                c.iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum()
            })
            .unwrap_or(0)
    };
    let mut widths: Vec<usize> = (0..n_cols)
        .map(|i| {
            let mut w = nat_w(&head_cells, i);
            for r in &body_rows {
                w = w.max(nat_w(r, i));
            }
            w.max(1)
        })
        .collect();
    // Frame columns: "│ " + ("…" + " │ ") * (n-1) + "…" + " │"  =  3n + 1.
    let frame = 3 * n_cols + 1;
    let avail = budget.saturating_sub(frame);
    let total: usize = widths.iter().sum();
    if total > avail && total > 0 {
        let scale = avail as f32 / total as f32;
        for w in widths.iter_mut() {
            *w = ((*w as f32) * scale).floor().max(1.0) as usize;
        }
    }

    let mut out = Vec::with_capacity(2 + body_rows.len());
    if !head_cells.is_empty() {
        out.push(render_table_row(&head_cells, &widths, &aligns));
    }
    out.push(render_table_rule(&widths));
    for r in &body_rows {
        out.push(render_table_row(r, &widths, &aligns));
    }
    out
}

fn render_table_row(
    row: &[Vec<Span<'static>>],
    widths: &[usize],
    aligns: &[Alignment],
) -> Line<'static> {
    let bar = |s: &'static str| Span::styled(s, Style::default().fg(SLATE));
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(widths.len() * 4 + 2);
    spans.push(bar("│ "));
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            spans.push(bar(" │ "));
        }
        let empty = Vec::new();
        let cell = row.get(i).unwrap_or(&empty);
        let cell_w: usize = cell
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let pad = w.saturating_sub(cell_w.min(w));
        let (lp, rp) = match aligns.get(i).copied().unwrap_or(Alignment::Left) {
            Alignment::Right => (pad, 0),
            Alignment::Center => (pad / 2, pad - pad / 2),
            _ => (0, pad),
        };
        if lp > 0 {
            spans.push(Span::raw(" ".repeat(lp)));
        }
        spans.extend(clip_spans(cell, w));
        if rp > 0 {
            spans.push(Span::raw(" ".repeat(rp)));
        }
    }
    spans.push(bar(" │"));
    Line::from(spans)
}

fn render_table_rule(widths: &[usize]) -> Line<'static> {
    let mut s = String::from("├");
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            s.push('┼');
        }
        s.push_str(&"─".repeat(w + 2));
    }
    s.push('┤');
    Line::from(Span::styled(s, Style::default().fg(SLATE)))
}

/// Clip a styled span sequence to at most `w` columns, replacing the tail
/// with `…` when truncation occurs.  Spans that fit verbatim are passed
/// through unchanged so cell styling is preserved.
fn clip_spans(spans: &[Span<'static>], w: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut used = 0;
    for s in spans {
        if used >= w {
            break;
        }
        let cw = UnicodeWidthStr::width(s.content.as_ref());
        if used + cw <= w {
            out.push(s.clone());
            used += cw;
            continue;
        }
        let rem = w.saturating_sub(used).saturating_sub(1);
        let mut buf = String::new();
        let mut bw = 0;
        for ch in s.content.chars() {
            let cwidth = UnicodeWidthChar::width(ch).unwrap_or(0);
            if bw + cwidth > rem {
                break;
            }
            buf.push(ch);
            bw += cwidth;
        }
        buf.push('…');
        out.push(Span::styled(buf, s.style));
        break;
    }
    out
}

// ── syntax highlighting ──────────────────────────────────────────────────

fn syntax_set() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| {
        two_face::theme::extra()
            .get(two_face::theme::EmbeddedThemeName::Nord)
            .clone()
    })
}

fn highlight_block(body: &str, lang: Option<&str>) -> Vec<Line<'static>> {
    let ss = syntax_set();
    let syntax = lang
        .and_then(|l| {
            ss.find_syntax_by_token(l)
                .or_else(|| ss.find_syntax_by_name(l))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for line in body.split_inclusive('\n') {
        let regions = hl.highlight_line(line, ss).unwrap_or_default();
        let spans: Vec<Span<'static>> = regions
            .into_iter()
            .filter_map(|(st, frag)| {
                let frag = frag.trim_end_matches('\n');
                (!frag.is_empty()).then(|| Span::styled(frag.to_string(), syn_to_ratatui(st)))
            })
            .collect();
        out.push(Line::from(spans));
    }
    while out.last().is_some_and(is_blank) {
        out.pop();
    }
    out
}

fn syn_to_ratatui(s: SynStyle) -> Style {
    let SynColor { r, g, b, .. } = s.foreground;
    let mut style = Style::default().fg(Color::Rgb(r, g, b));
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The viewport streamer commits one paragraph at a time at `\n\n`;
    /// the renderer must leave exactly one trailing blank so successive
    /// commits land separated rather than flush against each other.
    #[test]
    fn render_keeps_one_trailing_blank_on_blank_input() {
        let out = render_md("Paragraph.\n\n", 80, 0, Fidelity::default());
        assert!(!out.is_empty());
        assert!(
            is_blank(out.last().unwrap()),
            "tail must be a separator blank"
        );
        assert!(out.len() >= 2);
        assert!(!is_blank(&out[out.len() - 2]), "exactly one trailing blank");
    }

    /// Two paragraphs in one render keep their internal blank and gain a
    /// single trailing one — same shape as if they had been committed
    /// sequentially.
    #[test]
    fn render_keeps_internal_paragraph_break() {
        let out = render_md("Para one.\n\nPara two.\n\n", 80, 0, Fidelity::default());
        let internal = out[..out.len() - 1].iter().filter(|l| is_blank(l)).count();
        assert_eq!(internal, 1, "expected one internal blank, got {internal}");
        assert!(is_blank(out.last().unwrap()));
    }

    #[test]
    fn render_paragraph_has_no_trailing_blank_when_input_lacks_one() {
        let out = render_md("Paragraph.", 80, 0, Fidelity::default());
        assert!(!out.is_empty());
        assert!(!is_blank(out.last().unwrap()));
    }

    /// A sound fidelity leaves the rendering untouched — no DIM, no
    /// recoloured foreground.  The seed signal must cost nothing when the
    /// model is not under pressure.
    #[test]
    fn sound_fidelity_is_the_identity() {
        let plain = render_md("Some prose.", 80, 0, Fidelity::default());
        let stressed = render_md(
            "Some prose.",
            80,
            0,
            Fidelity {
                context: 0,
                echo: 0,
            },
        );
        assert_eq!(plain, stressed);
        for line in &plain {
            for span in &line.spans {
                assert!(!span.style.add_modifier.contains(Modifier::DIM));
            }
        }
    }

    /// Context level 2 dims every text span and pulls its foreground —
    /// including plain prose, which had no explicit colour and so gains
    /// the seeded base foreground pulled toward the background.
    #[test]
    fn context_pressure_dims_and_pulls_contrast() {
        let out = render_md(
            "Some prose here.",
            80,
            0,
            Fidelity {
                context: 2,
                echo: 0,
            },
        );
        let span = out
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| !s.content.trim().is_empty())
            .expect("a text span");
        assert!(
            span.style.add_modifier.contains(Modifier::DIM),
            "level 2 dims"
        );
        let pulled = span.style.fg.expect("a pulled foreground");
        // Pulled 40% from the base toward the dark background, so it sits
        // strictly between the two.
        let Color::Rgb(r, _, _) = pulled else {
            panic!("expected an RGB foreground");
        };
        let Color::Rgb(br, _, _) = BASE_FG else {
            unreachable!()
        };
        let Color::Rgb(cr, _, _) = CODE_BG else {
            unreachable!()
        };
        assert!(cr < r && r < br, "fg pulled toward bg: {cr} < {r} < {br}");
    }

    /// An echoed paragraph wavers: alternate non-blank rows carry
    /// foregrounds shifted in opposite lightness directions, so the block
    /// reads as unsteady.  A multi-row paragraph makes the oscillation
    /// observable.
    #[test]
    fn echo_wavers_alternate_rows() {
        // Force several rows by wrapping at a narrow width.
        let src = "one two three four five six seven eight nine ten eleven twelve";
        let out = render_md(
            src,
            12,
            0,
            Fidelity {
                context: 0,
                echo: 2,
            },
        );
        let fgs: Vec<Color> = out
            .iter()
            .filter(|l| !is_blank(l))
            .filter_map(|l| l.spans.iter().find(|s| !s.content.trim().is_empty()))
            .map(|s| s.style.fg.expect("echo seeds a foreground"))
            .collect();
        assert!(fgs.len() >= 2, "need multiple rows to observe the waver");
        assert_ne!(fgs[0], fgs[1], "adjacent rows waver in opposite directions");
    }
}
