//! Neon transcript decoration.
//!
//! Truecolor ANSI, plain `eprintln!`/`println!` direct to the tty.  Each
//! helper writes a self-contained block so the on-screen layout reads as
//! a sequence of labelled frames rather than a flat log.
//!
//! Assistant text streams in markdown blocks: each chunk extends an
//! `open` block whose live render is repainted in place via cursor-up
//! plus clear-to-end.  The repaint span is bounded by the *block* (a
//! paragraph, list, or fenced code) rather than the whole response, so
//! it always fits in the viewport — sidestepping the scroll-out problem
//! a whole-response repaint had.  Block boundaries are blank lines
//! outside a fenced code block.  Closed blocks carry a plain cyan rail;
//! the open block's rail steps through hues that rotate per chunk, so
//! the user sees a visible "still arriving" cue without per-character
//! recolouring.  See https://willmcgugan.github.io/streaming-markdown/
//! for the wider technique.

use std::io::{self, Write};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITAL: &str = "\x1b[3m";

fn rgb(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

fn pink() -> String   { rgb(255, 20, 147) }
fn cyan() -> String   { rgb(0, 240, 255) }
fn lime() -> String   { rgb(57, 255, 20) }
fn yellow() -> String { rgb(255, 234, 0) }
fn gold() -> String   { rgb(255, 191, 0) }
fn purple() -> String { rgb(191, 64, 255) }
fn orange() -> String { rgb(255, 95, 31) }
fn red() -> String    { rgb(255, 50, 80) }
fn slate() -> String  { rgb(140, 150, 170) }

/// Render width: the terminal's column count, clamped to 80 for
/// readable line length.  Falls back to 80 if the tty size is
/// unavailable (redirected output, no controlling terminal).
fn width() -> usize {
    termimad::crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .min(80)
}

const ART: &str = include_str!("../data/banner.txt");
const EAGLE: &str = include_str!("../data/eagle.txt");

pub fn banner(
    provider: &str,
    model: &str,
    system_size: usize,
    system_files: &[std::path::PathBuf],
    base: &str,
    extend_base: Option<&std::path::Path>,
    restrict_files: &[std::path::PathBuf],
    scratch: &std::path::Path,
) {
    let p = pink();
    let c = cyan();
    let l = lime();
    let s = slate();
    let o = orange();
    let r = red();
    let g = gold();
    eprintln!();
    for (a, e) in ART.lines().zip(EAGLE.lines()) {
        eprintln!("{p}{BOLD}{a}{RESET}  {g}{BOLD}{e}{RESET}");
    }
    eprintln!(" {s}{ITAL}a delegate driving ral under a grant{RESET}");
    eprintln!(
        "{s}provider {RESET}{c}{BOLD}{provider}{RESET}    {s}model {RESET}{l}{BOLD}{model}{RESET}"
    );
    // Base name; tint `dangerous` red so it's visually loud.
    let base_color = if base == "dangerous" { &r } else { &o };
    let path_list = |paths: &[std::path::PathBuf]| -> String {
        paths
            .iter()
            .map(|p| format!("{o}{BOLD}{}{RESET}", p.display()))
            .collect::<Vec<_>>()
            .join(&format!("{s}, {RESET}"))
    };
    let none = || format!("{s}none{RESET}");
    let extend_label = extend_base
        .map(|p| format!("{o}{BOLD}{}{RESET}", p.display()))
        .unwrap_or_else(none);
    let restrict_label = if restrict_files.is_empty() {
        none()
    } else {
        path_list(restrict_files)
    };
    eprintln!(
        "{s}base {RESET}{base_color}{BOLD}{base}{RESET}    {s}extend-base {RESET}{extend_label}    {s}restrict {RESET}{restrict_label}"
    );
    let size = format!("{:.1} kB", system_size as f64 / 1024.0);
    let source = if system_files.is_empty() {
        format!("{s}default{RESET}")
    } else {
        path_list(system_files)
    };
    eprintln!(
        "{s}system prompt {RESET}{l}{BOLD}{size}{RESET} {s}·{RESET} {source}"
    );
    eprintln!(
        "{s}scratch {RESET}{o}{BOLD}{}{RESET}",
        scratch.display()
    );
    eprintln!("{}{}{RESET}", purple(), "━".repeat(width()));
}

pub fn turn(n: usize) {
    let p = purple();
    let s = slate();
    eprintln!();
    eprintln!(
        "{p}◆ turn {BOLD}{n:02}{RESET} {s}{}{RESET}",
        "─".repeat(width().saturating_sub(10))
    );
}

/// Termimad skin shared by the live and final block painters — the
/// neon palette chosen to fit the surrounding UI.
fn skin() -> termimad::MadSkin {
    use termimad::crossterm::style::Color;
    use termimad::MadSkin;
    let mut s = MadSkin::default();
    s.bold.set_fg(Color::Rgb { r: 57, g: 255, b: 20 });
    s.italic.set_fg(Color::Rgb { r: 140, g: 150, b: 170 });
    s.inline_code.set_fg(Color::Rgb { r: 255, g: 95, b: 31 });
    s.code_block.compound_style.set_fg(Color::Rgb { r: 255, g: 95, b: 31 });
    s.headers[0].compound_style.set_fg(Color::Rgb { r: 255, g: 20, b: 147 });
    s.headers[1].compound_style.set_fg(Color::Rgb { r: 0, g: 240, b: 255 });
    s.headers[2].compound_style.set_fg(Color::Rgb { r: 255, g: 234, b: 0 });
    s
}

/// Render `text` as markdown wrapped to `inner` columns.  Blank input
/// yields an empty vec — useful because the streaming painter calls
/// this whenever its open buffer is whitespace-only.
fn render_markdown(text: &str, inner: usize) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    skin().text(text, Some(inner)).to_string().lines().map(String::from).collect()
}

pub fn tool_call(cmd: &str, audit: bool) {
    let p = pink();
    let y = yellow();
    let tag = if audit { "▶ shell+audit" } else { "▶ shell" };
    eprintln!();
    eprintln!("{p}{tag}{RESET}  {BOLD}{y}{}{RESET}", first_line(cmd));
    for l in cmd.lines().skip(1) {
        eprintln!("{p}│{RESET}        {y}{l}{RESET}");
    }
}

pub fn tool_result(out: &str) {
    let l = lime();
    eprintln!("{l}╎{RESET}");
    for src in out.lines() {
        let (tint, body) = match src {
            "STDOUT:" => (l.clone(), DIM),
            "STDERR:" => (orange(), DIM),
            s if s.starts_with("EXIT:") => {
                let colour = if s.trim_end().ends_with('0') { l.clone() } else { red() };
                eprintln!("{l}╎{RESET} {BOLD}{colour}{src}{RESET}");
                continue;
            }
            "VALUE:" => (purple(), DIM),
            s if s.starts_with("[+ audit tree") => {
                eprintln!("{l}╎{RESET} {}{src}{RESET}", purple());
                continue;
            }
            _ => (slate(), DIM),
        };
        eprintln!("{l}╎{RESET} {tint}{body}{src}{RESET}");
    }
    eprintln!("{l}╎{RESET}");
}

pub fn error(msg: &str) {
    eprintln!();
    eprintln!("{}{BOLD}✗ error{RESET} {msg}", red());
}

/// One-line task + session token / cost summary shown at the end of a
/// response.  Slate frame, lime numbers for the task, purple for total.
/// Cache fields are shown only when non-zero.
pub fn cost_summary(task: &crate::api::Usage, total: &crate::api::Usage) {
    let s = slate();
    let l = lime();
    let pu = purple();
    let t = |n: u64| if n >= 10_000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() };
    let d = |x: f64| if x > 0.0 { format!("${x:.4}") } else { "—".into() };
    let cache_suffix = |u: &crate::api::Usage| -> String {
        if u.cache_creation == 0 && u.cache_read == 0 {
            String::new()
        } else {
            format!(
                "{s} [{RESET}{}{} wr{RESET}{s}/{RESET}{}{} rd{RESET}{s}]{RESET}",
                l, t(u.cache_creation), l, t(u.cache_read),
            )
        }
    };
    eprintln!(
        "{s}┄ task {RESET}{l}{} in{RESET}{s} / {RESET}{l}{} out{RESET}{}{s} · {RESET}{BOLD}{l}{}{RESET}{s}   total {RESET}{pu}{} in{RESET}{s} / {RESET}{pu}{} out{RESET}{}{s} · {RESET}{BOLD}{pu}{}{RESET}",
        t(task.input), t(task.output), cache_suffix(task), d(task.dollars),
        t(total.input), t(total.output), cache_suffix(total), d(total.dollars),
    );
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// Block-level streaming painter for assistant text.
///
/// Tokens accumulate into an `open` markdown block; the live render of
/// that block is repainted in place after every chunk by walking the
/// cursor up over the previous render.  The repaint span is bounded by
/// the block — typically a single paragraph, list, or fenced code —
/// so it always fits in the viewport.  When a block boundary arrives
/// (blank line outside a fence) the open block is finalised: rendered
/// once with a plain cyan rail and left alone, and a fresh open block
/// begins.  The open block's rail steps through hues per row and
/// rotates per chunk, so it reads as a moving rainbow column while
/// text is still arriving.
pub struct Streaming {
    /// Text in the currently-open block.  Already-finalised blocks have
    /// been written to the terminal and dropped from memory.
    open: String,
    /// Lines the live render of `open` currently occupies on screen —
    /// the rewind distance for the next repaint.
    rendered_lines: usize,
    /// Whether we're inside a fenced code block at the start of `open`.
    in_code: bool,
    /// Base hue for the open block's rail; advanced per chunk.
    hue: f32,
    /// Render width captured at construction so the wrap math is stable
    /// even if the user resizes mid-stream.
    width: usize,
}

impl Default for Streaming {
    fn default() -> Self { Self::new() }
}

impl Streaming {
    pub fn new() -> Self {
        Self {
            open: String::new(),
            rendered_lines: 0,
            in_code: false,
            hue: 0.0,
            width: width(),
        }
    }

    /// Append a delta from the model, finalise any blocks that closed
    /// inside it, and repaint whatever's left as the live open block.
    pub fn push(&mut self, s: &str) {
        if s.is_empty() { return; }
        self.open.push_str(s);
        let mut out = io::stdout().lock();
        while let Some((content_end, consumed, in_code_after)) =
            find_block_break(&self.open, self.in_code)
        {
            self.erase_open(&mut out);
            let block = self.open[..content_end].to_string();
            paint_block(&mut out, &block, self.width, RailStyle::Final);
            self.open.drain(..consumed);
            self.in_code = in_code_after;
        }
        self.erase_open(&mut out);
        if !self.open.trim().is_empty() {
            self.hue = (self.hue + 30.0) % 360.0;
            self.rendered_lines = paint_block(
                &mut out,
                &self.open,
                self.width,
                RailStyle::Live(self.hue),
            );
        }
        let _ = out.flush();
    }

    /// Finalise the open block, if any, and flush.
    pub fn finish(mut self) {
        let mut out = io::stdout().lock();
        self.erase_open(&mut out);
        if !self.open.trim().is_empty() {
            paint_block(&mut out, &self.open, self.width, RailStyle::Final);
        }
        let _ = out.flush();
    }

    /// CSI nA + CSI 0J: rewind to the top of the live render and clear
    /// from there to end of screen.  Bounded by block size, so the
    /// rewind always lands within the viewport.
    fn erase_open<W: Write>(&mut self, out: &mut W) {
        if self.rendered_lines > 0 {
            let _ = write!(out, "\r\x1b[{}A\x1b[0J", self.rendered_lines);
            self.rendered_lines = 0;
        }
    }
}

#[derive(Clone, Copy)]
enum RailStyle {
    /// Solid cyan — used once a block is sealed.
    Final,
    /// Rainbow rail with a base hue; successive rows step the hue, so
    /// the rail reads as a column that shifts each time the chunk hue
    /// rotates.
    Live(f32),
}

/// Render `text` as markdown, prefix each physical line with a rail in
/// the requested style, and return the line count.
fn paint_block<W: Write>(out: &mut W, text: &str, width: usize, style: RailStyle) -> usize {
    let lines = render_markdown(text, width.saturating_sub(2));
    for (i, line) in lines.iter().enumerate() {
        let rail = match style {
            RailStyle::Final => cyan(),
            RailStyle::Live(base) => {
                let (r, g, b) = hsv(base + 25.0 * i as f32);
                rgb(r, g, b)
            }
        };
        let _ = writeln!(out, "{rail}┃{RESET} {line}");
    }
    lines.len()
}

/// Find the first blank line in `text` that lies outside a fenced code
/// block, given the entry-state `in_code`.  Returns
/// `(content_end, consumed, in_code_after)` where `content_end` is the
/// byte index where block content ends, `consumed` is how many bytes to
/// drain to skip past the trailing blank line(s), and `in_code_after`
/// is the fence state at the end of the block content.  `None` if no
/// complete break has arrived yet — the caller keeps the buffer open.
fn find_block_break(text: &str, in_code_in: bool) -> Option<(usize, usize, bool)> {
    let mut in_code = in_code_in;
    let mut had_content = false;
    let mut bytes = 0;
    while bytes < text.len() {
        let line_end = bytes + text[bytes..].find('\n')? + 1;
        let line = &text[bytes..line_end];
        let content = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = content.trim();
        if !in_code && trimmed.is_empty() && had_content {
            // Coalesce any further blank lines into the same boundary so
            // a `\n\n\n` between blocks doesn't leave a stray rail row.
            let mut consumed = line_end;
            while let Some(i) = text.get(consumed..).and_then(|t| t.find('\n')) {
                let nxt = &text[consumed..consumed + i + 1];
                if nxt.strip_suffix('\n').unwrap_or(nxt).trim().is_empty() {
                    consumed += i + 1;
                } else {
                    break;
                }
            }
            return Some((bytes, consumed, in_code));
        }
        if content.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        if !trimmed.is_empty() {
            had_content = true;
        }
        bytes = line_end;
    }
    None
}

/// HSV→RGB at saturation 1.0 and value 1.0 — the neon end of the
/// palette.  `h` in degrees, wrapped into 0..360.
fn hsv(h: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn break_after_paragraph() {
        assert_eq!(find_block_break("hello\n\nworld", false), Some((6, 7, false)));
    }

    #[test]
    fn no_break_inside_fence() {
        // Blank line inside an opened fence is not a boundary; the boundary
        // only fires after the closing fence.
        assert_eq!(
            find_block_break("```\na\n\nb\n```\n\nout", false),
            Some((13, 14, false)),
        );
    }

    #[test]
    fn no_break_yet_partial() {
        assert_eq!(find_block_break("hello world", false), None);
        assert_eq!(find_block_break("hello\nworld\n", false), None);
    }

    #[test]
    fn no_break_when_started_inside_fence() {
        // Starting in_code = true: the blank line is part of the code
        // block, so no boundary until the fence closes.
        assert_eq!(find_block_break("a\n\nb", true), None);
    }

    #[test]
    fn coalesces_repeated_blank_lines() {
        // Three newlines after content: the block content (including its
        // own terminating \n) is 6 bytes, and the boundary consumes both
        // blank lines so the next block starts cleanly at "next".
        let r = find_block_break("hello\n\n\nnext", false);
        assert_eq!(r, Some((6, 8, false)));
    }
}
