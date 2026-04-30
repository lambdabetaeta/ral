//! Neon transcript decoration.
//!
//! Truecolor ANSI, plain `eprintln!`/`println!` direct to the tty.  Each
//! helper writes a self-contained block so the on-screen layout reads as
//! a sequence of labelled frames rather than a flat log.
//!
//! Assistant text is rendered as Markdown via `termimad` before the
//! per-line cyan-bar prefix is applied.  Live tokens stream through
//! `Streaming` with a drifting rainbow hue, then get repainted as
//! markdown on close — a deliberately simple stand-in for the
//! block-incremental approach Will McGugan describes in
//! https://willmcgugan.github.io/streaming-markdown/ (see also
//! dev/streaming_markdown.md).

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

pub fn assistant_text(text: &str) {
    use termimad::crossterm::style::Color;
    use termimad::MadSkin;

    let mut skin = MadSkin::default();
    skin.bold.set_fg(Color::Rgb { r: 57, g: 255, b: 20 });
    skin.italic.set_fg(Color::Rgb { r: 140, g: 150, b: 170 });
    skin.inline_code.set_fg(Color::Rgb { r: 255, g: 95, b: 31 });
    skin.code_block.compound_style.set_fg(Color::Rgb { r: 255, g: 95, b: 31 });
    skin.headers[0].compound_style.set_fg(Color::Rgb { r: 255, g: 20, b: 147 });
    skin.headers[1].compound_style.set_fg(Color::Rgb { r: 0, g: 240, b: 255 });
    skin.headers[2].compound_style.set_fg(Color::Rgb { r: 255, g: 234, b: 0 });

    let rendered = skin.text(text, Some(width() - 2)).to_string();
    let c = cyan();
    for line in rendered.lines() {
        println!("{c}┃{RESET} {line}");
    }
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

/// Live painter for streamed assistant tokens.
///
/// Each character is written with a hue that drifts a few degrees per
/// glyph, prefixed with the same cyan rail `assistant_text` uses.  On
/// `finish` we walk the cursor back over every line we wrote and
/// re-emit the full text through `assistant_text`, so the terminal
/// settles on the proper termimad render — markdown, bold, code, the
/// lot.  The rainbow is just to telegraph "this text is still
/// arriving".
pub struct Streaming {
    /// Newline-terminated lines emitted so far.
    lines: usize,
    /// Characters since the last newline, exclusive of the rail prefix.
    col: usize,
    /// Hue in degrees, advanced per character.
    hue: f32,
    /// Accumulated raw text — the source for the closing repaint.
    text: String,
    /// Render width captured at construction so the soft-wrap column
    /// stays stable across the stream — the cursor-up math at
    /// `finish()` depends on it.
    width: usize,
}

impl Default for Streaming {
    fn default() -> Self { Self::new() }
}

impl Streaming {
    pub fn new() -> Self {
        Self { lines: 0, col: 0, hue: 0.0, text: String::new(), width: width() }
    }

    /// Append a delta from the model.  Writes to stdout as it goes so
    /// the user sees motion before the response is complete.
    pub fn push(&mut self, s: &str) {
        if s.is_empty() { return; }
        self.text.push_str(s);
        let mut out = io::stdout().lock();
        for ch in s.chars() {
            self.put(&mut out, ch);
        }
        let _ = out.flush();
    }

    /// Walk back over the streamed region, clear it, and repaint the
    /// final markdown render in its place.  No-op if nothing was
    /// streamed.
    pub fn finish(mut self) {
        if self.text.is_empty() { return; }
        let mut out = io::stdout().lock();
        if self.col > 0 {
            let _ = writeln!(out, "{RESET}");
            self.lines += 1;
        }
        if self.lines > 0 {
            // CSI nA: cursor up n; CSI 0J: clear from cursor to end of
            // screen.  Together they erase exactly the streamed region.
            let _ = write!(out, "\x1b[{}A\x1b[0J", self.lines);
            let _ = out.flush();
        }
        drop(out);
        assistant_text(&self.text);
    }

    fn put<W: Write>(&mut self, out: &mut W, ch: char) {
        if self.col == 0 {
            let _ = write!(out, "{}┃{RESET} ", cyan());
        }
        if ch == '\n' {
            let _ = writeln!(out, "{RESET}");
            self.col = 0;
            self.lines += 1;
            return;
        }
        let (r, g, b) = hsv(self.hue);
        let _ = write!(out, "\x1b[38;2;{r};{g};{b}m{ch}");
        self.hue = (self.hue + 4.0) % 360.0;
        self.col += 1;
        if self.col >= self.width - 2 {
            let _ = writeln!(out, "{RESET}");
            self.col = 0;
            self.lines += 1;
        }
    }
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
