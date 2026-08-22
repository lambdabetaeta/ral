//! Markdown to ratatui lines: `pulldown-cmark` events walked straight into
//! spans.  A ral code block is coloured by [`super::highlight`], the same
//! lexer-backed pass the tool-call panels use, so the language looks the same
//! wherever it appears; any other language falls to syntect and the
//! `two-face` syntax set.
//!
//! `super::viewport` commits only fence-safe paragraph prefixes, so every call
//! here sees a structurally complete slice and needs no repair pass.

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::iter::once;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SynColor, FontStyle, Style as SynStyle, Theme};
use syntect::parsing::SyntaxSet;
use tui_math::MathRenderer;
use unicode_width::UnicodeWidthStr;

use super::fidelity::Fidelity;
use super::highlight::highlight_ral;
use super::line::{is_blank, wrap_line};
use super::palette::{CYAN, LIME, READ_W, SLATE};
use super::rail::{desaturate, mix};

/// Left inset for assistant markdown, holding the model's voice off the chrome.
pub(super) const MD_INDENT: u16 = 4;

/// The foreground assumed for uncoloured spans — the drain needs a hue to take.
const BASE_FG: Color = Color::Rgb(208, 213, 224);

/// Left inset for a display formula, setting the notation off from the prose.
const MATH_INSET: usize = 2;

/// The flat wash behind an echoed paragraph, one step deeper per echo level.
/// Static across rows, so it reads as a flagged passage, not a render glitch.
const ECHO_WASH: Color = Color::Rgb(46, 40, 54);

// ── public entry points ──────────────────────────────────────────────────

/// Render `text` to lines clamped to [`READ_W`] columns.  `indent` both shrinks
/// the wrap budget and prefixes every non-blank line, insetting the prose.
pub(super) fn render_md(text: &str, w: u16, indent: u16, fidelity: Fidelity) -> Vec<Line<'static>> {
    let body_w = w.min(READ_W).saturating_sub(indent).max(1) as usize;
    let mut comp = Composer::new(body_w, indent as usize);
    let mut p = Parser::new_ext(text, gfm());
    while let Some(ev) = p.next() {
        comp.event(ev, &mut p);
    }
    // A paragraph break in the input keeps one blank at the tail.
    let mut lines = comp.finish(text.ends_with("\n\n"));
    modulate(&mut lines, fidelity);
    lines
}

/// Saturation drained from reasoning prose: scratch work, short of illegible.
const REASONING_DRAIN: f32 = 0.7;
/// Dimming applied after the drain, so reasoning drops in luminance too.
const REASONING_DIM: f32 = 0.35;
/// Render reasoning prose, drained and dimmed so it never borrows the answer's
/// authority — independent of [`Fidelity`], since thinking is always provisional.
pub(super) fn render_reasoning(text: &str, w: u16, indent: u16) -> Vec<Line<'static>> {
    let mut lines = render_md(text, w, indent, Fidelity::default());
    for line in &mut lines {
        for span in &mut line.spans {
            let fg = desaturate(span.style.fg.unwrap_or(BASE_FG), REASONING_DRAIN);
            span.style.fg = Some(mix(fg, Color::Rgb(80, 80, 80), REASONING_DIM));
        }
    }
    lines
}

/// Degrade every built span, code and tables included, so the ink tracks the
/// passage's reliability — never through value, which carries magnitude on the rail.
fn modulate(lines: &mut [Line<'static>], f: Fidelity) {
    if f.context == 0 && f.echo == 0 {
        return;
    }
    for line in lines.iter_mut() {
        for span in &mut line.spans {
            drain_span(span, f.context, f.echo);
        }
    }
}

/// Saturation drained per context level, toward luma-grey so legibility lives.
/// The deepest step stands for every level past it: distress saturates.
const DRAIN: [f32; 4] = [0.0, 0.45, 0.70, 0.90];

/// Drain a span's foreground and, when the block echoes, wash the field behind
/// it.  Shared by [`modulate`] and [`apply_context`]: prose and intent degrade alike.
fn drain_span(span: &mut Span<'static>, context: u8, echo: u8) {
    if span.content.trim().is_empty() {
        return;
    }
    let drain = DRAIN[usize::from(context).min(DRAIN.len() - 1)];
    if drain > 0.0 {
        let fg = span.style.fg.unwrap_or(BASE_FG);
        span.style.fg = Some(desaturate(fg, drain));
    }
    if echo > 0 {
        span.style.bg = Some(mix(Color::Rgb(0, 0, 0), ECHO_WASH, f32::from(echo) / 2.0));
    }
}

/// Degrade a ral group's intent line ([`super::group`]) by the turn's context
/// floor — distress on the ink, never a bar's height.  An intent takes no wash.
pub(super) fn apply_context(line: &mut Line<'static>, context: u8) {
    for span in &mut line.spans {
        drain_span(span, context, 0);
    }
}

fn gfm() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_MATH
}

/// Drop trailing blanks, so prose and fenced code share one spacing baseline.
fn trim_trailing_blanks(lines: &mut Vec<Line<'static>>) {
    while lines.last().is_some_and(is_blank) {
        lines.pop();
    }
}

// ── composer ─────────────────────────────────────────────────────────────

/// Walks pulldown-cmark events into lines, holding wrap width, style stack, lists
/// and rails.  Fences and tables render apart, rejoining via [`Self::append_rendered`].
struct Composer {
    out: Vec<Line<'static>>,
    /// The open line, unwrapped: [`Self::flush_line`] folds it to the budget.
    cur: Vec<Span<'static>>,
    body_w: usize,
    indent: usize,
    style: Style,
    style_stack: Vec<Style>,
    /// Active list containers, innermost last.
    list_stack: Vec<ListCtx>,
    /// Blockquote nesting depth — each level paints one `│ ` slate rail.
    rail_depth: usize,
    /// Open inline links — pop on `TagEnd::Link` to maybe emit `(url)`.
    links: Vec<(LinkType, String)>,
}

struct ListCtx {
    /// `None` for unordered; `Some(n)` is the next ordered marker.
    next: Option<u64>,
    /// Column width reserved for the marker, so continuations align under it.
    marker_w: usize,
    /// The current item's first-line marker, consumed by `left_margin`.
    pending: Option<Span<'static>>,
}

impl Composer {
    fn new(body_w: usize, indent: usize) -> Self {
        Self {
            out: Vec::new(),
            cur: Vec::new(),
            body_w,
            indent,
            style: Style::default(),
            style_stack: Vec::new(),
            list_stack: Vec::new(),
            rail_depth: 0,
            links: Vec::new(),
        }
    }

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
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => self.text(&t),
            Event::InlineMath(t) => self.inline_math(&t),
            Event::DisplayMath(t) => self.display_math(&t),
            Event::Code(t) => {
                self.push_span(Span::styled(t.into_string(), Style::default().fg(LIME)));
            }
            // A source line break is one space of prose, nothing more.
            Event::SoftBreak => self.text(" "),
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
                let lines = render_table(p, &aligns, self.budget());
                self.append_rendered(lines);
                self.blank_separator();
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}

            // A link's URL is stashed so `TagEnd::Link` can trail `(url)`.
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
            TagEnd::CodeBlock
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell => {}
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                self.blank_separator();
            }
            // A tight list's item text arrives bare, so the flush is all it needs;
            // a loose one wraps items in `Paragraph`, which laid the blank already.
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
        }
    }

    // ── inline ──

    /// Inline prose, word runs and their separating gaps kept apart as spans so
    /// the fold downstream can tell one from the other.
    fn text(&mut self, t: &str) {
        for part in t.split_inclusive(char::is_whitespace) {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            if !trimmed.is_empty() {
                self.push_span(Span::styled(trimmed.to_string(), self.style));
            }
            if part.len() != trimmed.len() {
                self.push_span(Span::styled(" ".to_string(), self.style));
            }
        }
    }

    /// Inline `$…$`.  A one-row grid is notation and joins the sentence; a taller
    /// one would have to break the paragraph to draw itself, so the source
    /// stands in instead.
    fn inline_math(&mut self, latex: &str) {
        match math_rows(latex, self.budget()).as_deref() {
            Some([row]) => self.push_span(Span::styled(row.clone(), self.style)),
            _ => self.literal_math(latex),
        }
    }

    /// Display `$$…$$`: a block of its own, inset so it reads as set apart from
    /// the paragraph even where the grid comes out one row tall.
    fn display_math(&mut self, latex: &str) {
        self.block_break();
        match math_rows(latex, self.budget().saturating_sub(MATH_INSET)) {
            Some(rows) => {
                let (style, inset) = (self.style, Span::raw(" ".repeat(MATH_INSET)));
                self.append_rendered(
                    rows.into_iter()
                        .map(|row| Line::from(vec![inset.clone(), Span::styled(row, style)]))
                        .collect(),
                );
            }
            None => self.literal_math(latex),
        }
        self.block_break();
    }

    /// The LaTeX itself, inked as the literal it is — the rendering of a
    /// formula this pass will not draw.
    fn literal_math(&mut self, latex: &str) {
        self.push_style(Style::default().fg(LIME));
        self.text(latex);
        self.pop_style();
    }

    /// Append a span to the open line, measuring nothing: where the columns
    /// fall is [`Self::flush_line`]'s business, and it alone sees the whole
    /// paragraph.  Whitespace at the head of a line is dropped, so a soft break
    /// across a block boundary never inks a leading column.
    fn push_span(&mut self, span: Span<'static>) {
        if self.cur.is_empty() && span.content.trim().is_empty() {
            return;
        }
        self.cur.push(span);
    }

    fn rule(&mut self) {
        self.block_break();
        self.push_span(Span::styled(
            "─".repeat(self.budget()),
            Style::default().fg(SLATE),
        ));
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

    /// Flush the open line, then a blank unless one already sits at the tail.
    fn block_break(&mut self) {
        self.flush_line();
        self.blank_separator();
    }

    /// Fold the open line to the content budget and seat every row under the
    /// margin — the first row wearing the list marker, the rest the pad that
    /// holds their text under it.  [`wrap_line`] owns the fold: its word runs
    /// cross span seams, so a styled span and the punctuation fused to it
    /// (`` `code`. ``) break as one unit, and a span wider than the column
    /// breaks between characters instead of running off the row.
    fn flush_line(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let line = Line::from(std::mem::take(&mut self.cur));
        for row in wrap_line(&line, self.budget()) {
            let mut spans = self.left_margin();
            spans.extend(row.spans);
            self.out.push(Line::from(spans));
        }
    }

    /// Build the next line's left margin: chrome indent, one `│ ` per rail, then
    /// pad spaces and the innermost list's pending marker, which this consumes.
    fn left_margin(&mut self) -> Vec<Span<'static>> {
        let mut sp: Vec<Span<'static>> = Vec::new();
        if self.indent > 0 {
            sp.push(Span::raw(" ".repeat(self.indent)));
        }
        for _ in 0..self.rail_depth {
            sp.push(Span::styled("│ ", Style::default().fg(SLATE)));
        }
        let (marker, mw) = self
            .list_stack
            .last_mut()
            .map_or((None, 0), |c| (c.pending.take(), c.marker_w));
        // The marker occupies its own column width; only the rest is padding.
        let pad = self
            .pad()
            .saturating_sub(if marker.is_some() { mw } else { 0 });
        if pad > 0 {
            sp.push(Span::raw(" ".repeat(pad)));
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

    /// Stitch externally rendered lines (code, table) into `out` under the margin.
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

    /// Drain the open line, stripping trailing blanks but for an optional separator.
    fn finish(mut self, trailing_blank: bool) -> Vec<Line<'static>> {
        self.flush_line();
        trim_trailing_blanks(&mut self.out);
        if trailing_blank && !self.out.is_empty() {
            self.out.push(Line::default());
        }
        self.out
    }
}

/// Inline-tag → style delta; `None` for block and structural tags.  Shared by
/// [`Composer::start`] and [`render_table`] so the two walkers cannot drift.
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

/// Consume events through `End(Table)` into a boxed table.  Cell content past
/// its column wraps ([`super::line::wrap_line`]) rather than being clipped.
fn render_table<'a, I: Iterator<Item = Event<'a>>>(
    p: &mut I,
    aligns: &[Alignment],
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
            // Every tag inside a table is balanced — cells carry inline content
            // only — so pushing on each `Start` and popping on each `End` keeps
            // the stack in step without naming which tags bear a style.
            Event::Start(t) => {
                stack.push(style);
                if let Some(d) = style_delta(&t) {
                    style = style.patch(d);
                }
                match t {
                    Tag::TableHead => in_head = true,
                    Tag::TableRow => cur_row.clear(),
                    Tag::TableCell => cur_cell.clear(),
                    _ => {}
                }
            }
            Event::End(e) => {
                style = stack.pop().unwrap_or_default();
                match e {
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
                }
            }
            Event::Text(t) => cur_cell.push(Span::styled(t.into_string(), style)),
            Event::Code(t) => {
                cur_cell.push(Span::styled(t.into_string(), Style::default().fg(LIME)));
            }
            // A cell is one row, so a grid taller than one cannot live in it:
            // the same flat-or-literal rule the prose walker applies inline.
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                cur_cell.push(match math_rows(&t, budget).as_deref() {
                    Some([row]) => Span::styled(row.clone(), style),
                    _ => Span::styled(t.into_string(), Style::default().fg(LIME)),
                });
            }
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
        row.get(i).map_or(0, |c| {
            c.iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum()
        })
    };
    let mut widths: Vec<usize> = (0..n_cols)
        .map(|i| {
            once(nat_w(&head_cells, i))
                .chain(body_rows.iter().map(|r| nat_w(r, i)))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    // Chrome: "│ " + cell + (" │ " + cell) * (n-1) + " │"  =  3n + 1 columns.
    let frame = 3 * n_cols + 1;
    let avail = budget.saturating_sub(frame);
    let total: usize = widths.iter().sum();
    if total > avail && total > 0 {
        // Only the widest column yields, so narrow ones keep their natural width.
        while widths.iter().sum::<usize>() > avail {
            if let Some(i) = (0..n_cols)
                .filter(|&i| widths[i] > 1)
                .max_by_key(|&i| widths[i])
            {
                widths[i] -= 1;
            } else {
                break;
            }
        }
    }

    let mut out = Vec::new();
    if !head_cells.is_empty() {
        out.extend(render_table_row(&head_cells, &widths, aligns));
    }
    out.push(render_table_rule(&widths));
    for r in &body_rows {
        out.extend(render_table_row(r, &widths, aligns));
    }
    out
}

/// Render one row, as tall as its tallest cell; short cells pad to hold the frame.
fn render_table_row(
    row: &[Vec<Span<'static>>],
    widths: &[usize],
    aligns: &[Alignment],
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Line<'static>>> = widths
        .iter()
        .enumerate()
        .map(|(i, &w)| wrap_line(&Line::from(row.get(i).cloned().unwrap_or_default()), w))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

    let bar = |s: &'static str| Span::styled(s, Style::default().fg(SLATE));
    let blank = Line::default();
    let mut out = Vec::new();
    for li in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(bar("│ "));
        for (i, &w) in widths.iter().enumerate() {
            if i > 0 {
                spans.push(bar(" │ "));
            }
            let cell = wrapped[i].get(li).unwrap_or(&blank);
            let cell_w = cell.width();
            let pad = w.saturating_sub(cell_w);
            let (lp, rp) = match aligns.get(i).copied().unwrap_or(Alignment::Left) {
                Alignment::Right => (pad, 0),
                Alignment::Center => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            if lp > 0 {
                spans.push(Span::raw(" ".repeat(lp)));
            }
            spans.extend(cell.spans.iter().cloned());
            if rp > 0 {
                spans.push(Span::raw(" ".repeat(rp)));
            }
        }
        spans.push(bar(" │"));
        out.push(Line::from(spans));
    }
    out
}

fn render_table_rule(widths: &[usize]) -> Line<'static> {
    let cells: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
    Line::from(Span::styled(
        format!("├{}┤", cells.join("┼")),
        Style::default().fg(SLATE),
    ))
}

// ── syntax highlighting ──────────────────────────────────────────────────

/// Every syntax `two-face` ships, parsed once: the set is large and a fence is
/// rare, so it is built on the first non-ral block and never again.
static SYNTAX: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Nord, for its muted palette: a fence must not outshout the prose around it.
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    two_face::theme::extra()
        .get(two_face::theme::EmbeddedThemeName::Nord)
        .clone()
});

/// Fence tags that mean ral, plus the untagged case: the agent's only tool is
/// the shell, so a bare block in its prose is ral until it says otherwise, and
/// `ral.md` teaches the language in indented blocks, which carry no tag at all.
fn is_ral_block(lang: Option<&str>) -> bool {
    lang.is_none_or(|l| matches!(l.to_ascii_lowercase().as_str(), "ral" | "ral-sh"))
}

/// Lay `latex` out on a character grid, rows in reading order: fractions stacked
/// over a vinculum, roots under one, big operators carrying their limits.
/// `None` when the parser refuses the source or the grid overruns `budget` —
/// the LaTeX the model wrote is the honest rendering then, and it wraps.
fn math_rows(latex: &str, budget: usize) -> Option<Vec<String>> {
    let grid = MathRenderer::new().render_to_box(latex).ok()?;
    (1..=budget).contains(&grid.width).then(|| {
        grid.to_string()
            .lines()
            .map(str::trim_end)
            .map(String::from)
            .collect()
    })
}

fn highlight_block(body: &str, lang: Option<&str>) -> Vec<Line<'static>> {
    if is_ral_block(lang) {
        let mut out = highlight_ral(body);
        trim_trailing_blanks(&mut out);
        return out;
    }
    let ss = &*SYNTAX;
    let syntax = lang
        .and_then(|l| {
            ss.find_syntax_by_token(l)
                .or_else(|| ss.find_syntax_by_name(l))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, &THEME);
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
    trim_trailing_blanks(&mut out);
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

    #[allow(
        clippy::suboptimal_flops,
        reason = "u8-rounded colour math; mul_add adds no precision and obscures the standard lerp/luma formula"
    )]
    fn luma(c: Color) -> f32 {
        let Color::Rgb(r, g, b) = c else {
            unreachable!("test colours are RGB")
        };
        0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
    }

    /// Every non-blank span of a rendered block — the ink modulation acts on.
    fn ink<'a>(lines: &'a [Line<'static>]) -> Vec<&'a Span<'static>> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| !s.content.trim().is_empty())
            .collect()
    }

    /// Rendered text of a block, one string per row.
    fn rows(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// A display formula is typeset, not quoted: numerator over vinculum over
    /// denominator, each row its own line.
    #[test]
    fn display_math_stacks() {
        let rows = rows(&render_md(
            "$$\\frac{x^2 + 1}{y}$$",
            80,
            MD_INDENT,
            Fidelity::default(),
        ));
        let body: Vec<&String> = rows.iter().filter(|r| !r.trim().is_empty()).collect();
        assert_eq!(body.len(), 3, "numerator, vinculum, denominator: {rows:?}");
        assert!(body[0].contains("x²"), "superscript typeset: {body:?}");
        assert!(body[1].contains('─'), "a vinculum divides them: {body:?}");
    }

    /// Inline notation joins the sentence it belongs to, on one row.
    #[test]
    fn inline_math_joins_the_prose() {
        let rows = rows(&render_md(
            "the bound $x^2 + y_1$ holds",
            80,
            MD_INDENT,
            Fidelity::default(),
        ));
        assert_eq!(rows.len(), 1, "one row: {rows:?}");
        assert!(
            rows[0].contains("the bound x² + y₁ holds"),
            "typeset in place: {rows:?}"
        );
    }

    /// A formula that cannot be flattened keeps its source, inked as the literal
    /// it is — a stacked fraction would have to break the paragraph to draw.
    #[test]
    fn unflattenable_inline_math_keeps_its_source() {
        let lines = render_md(
            "the ratio $\\frac{a}{b}$ holds",
            80,
            MD_INDENT,
            Fidelity::default(),
        );
        let rows = rows(&lines);
        assert_eq!(rows.len(), 1, "the paragraph is intact: {rows:?}");
        assert!(
            rows[0].contains("\\frac{a}{b}"),
            "source stands in: {rows:?}"
        );
        let latex = ink(&lines)
            .into_iter()
            .find(|s| s.content.contains("frac"))
            .expect("the source is inked");
        assert_eq!(latex.style.fg, Some(LIME), "inked as a literal");
    }

    /// A table cell is one row, so a formula in one obeys the inline rule.
    #[test]
    fn table_cell_typesets_math() {
        let rows = rows(&render_md(
            "| bound |\n| --- |\n| $x^2$ |\n",
            80,
            MD_INDENT,
            Fidelity::default(),
        ));
        assert!(
            rows.iter().any(|r| r.contains("x²")),
            "the cell is typeset, not dropped: {rows:?}"
        );
    }

    /// The frame is pinned: one space of padding either side of the widest
    /// cell in each column, and a rule that spans exactly that padded width.
    #[test]
    fn table_frame_is_drawn_exactly() {
        let rows = rows(&render_md(
            "| a | bb |\n| --- | --- |\n| ccc | d |\n",
            80,
            MD_INDENT,
            Fidelity::default(),
        ));
        let frame: Vec<&str> = rows
            .iter()
            .map(|r| r.trim_start())
            .filter(|r| !r.is_empty())
            .collect();
        assert_eq!(
            frame,
            ["│ a   │ bb │", "├─────┼────┤", "│ ccc │ d  │"],
            "the drawn frame: {rows:?}"
        );
    }

    /// A span wider than the budget breaks between characters.  Wrapping only
    /// *before* it would run the tail off the row, where the terminal clips it.
    #[test]
    fn over_wide_span_keeps_its_tail() {
        let src = format!("prose `{}` more", "a".repeat(200));
        let rows = rows(&render_md(&src, 40, MD_INDENT, Fidelity::default()));
        for r in &rows {
            assert!(
                UnicodeWidthStr::width(r.as_str()) <= 40,
                "no row overruns the terminal: {r:?}"
            );
        }
        let kept: usize = rows.iter().map(|r| r.matches('a').count()).sum();
        assert_eq!(kept, 200, "every character survives: {rows:?}");
    }

    /// A styled span and the punctuation fused to it are one word: the fold
    /// falls before the pair, never at the seam, so no row opens on a lone `,`.
    #[test]
    fn fused_punctuation_breaks_as_one_word() {
        let rows = rows(&render_md(
            "alpha beta `gamma`, delta",
            MD_INDENT + 14,
            MD_INDENT,
            Fidelity::default(),
        ));
        assert!(rows.len() > 1, "expected a fold: {rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("gamma,")),
            "the span and its punctuation stayed whole: {rows:?}"
        );
        for r in &rows {
            assert!(
                !r.trim_start().starts_with(','),
                "punctuation orphaned onto its own row: {rows:?}"
            );
        }
    }

    /// A sound block is left alone: no wash, no carried-over `DIM`.
    #[test]
    fn sound_prose_is_untouched() {
        let lines = render_md("plain prose here", 80, MD_INDENT, Fidelity::default());
        for span in ink(&lines) {
            assert!(span.style.bg.is_none(), "sound prose wears no wash");
            assert!(!span.style.add_modifier.contains(Modifier::DIM));
        }
    }

    /// The drain holds luminance and adds no `DIM` — that idiom is for minor chrome.
    #[test]
    fn context_drains_without_dim() {
        let lines = render_md(
            "plain prose here",
            80,
            MD_INDENT,
            Fidelity {
                context: 2,
                echo: 0,
            },
        );
        let spans = ink(&lines);
        assert!(!spans.is_empty());
        for span in spans {
            let fg = span.style.fg.expect("drained span carries an explicit fg");
            assert!(
                (luma(fg) - luma(BASE_FG)).abs() <= 1.0,
                "drain held luminance"
            );
            assert_ne!(fg, BASE_FG, "drain desaturated the ink");
            assert!(
                !span.style.add_modifier.contains(Modifier::DIM),
                "drain must not borrow the minor-chrome DIM idiom"
            );
        }
    }

    /// One wash on every row — a flag, not a glitch — and the foreground untouched.
    #[test]
    fn echo_washes_background_statically() {
        let lines = render_md(
            "first line is long enough to wrap onto a second rendered row here please",
            40,
            MD_INDENT,
            Fidelity {
                context: 0,
                echo: 2,
            },
        );
        let spans = ink(&lines);
        assert!(
            spans.len() >= 2,
            "needs multiple rows to test row-invariance"
        );
        let washes: Vec<Color> = spans
            .iter()
            .map(|s| s.style.bg.expect("echoed span carries a wash"))
            .collect();
        let first = washes[0];
        assert!(
            washes.iter().all(|&w| w == first),
            "wash is static across rows"
        );
        assert_eq!(
            first,
            mix(Color::Rgb(0, 0, 0), ECHO_WASH, 1.0),
            "wash is the full echo shade"
        );
        for s in &spans {
            assert_eq!(s.style.fg, None, "echo leaves the foreground untouched");
        }
    }
}
