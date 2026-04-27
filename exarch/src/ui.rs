//! Neon transcript decoration.
//!
//! Truecolor ANSI, plain `eprintln!`/`println!` direct to the tty.  Each
//! helper writes a self-contained block so the on-screen layout reads as
//! a sequence of labelled frames rather than a flat log.
//!
//! Assistant text is rendered as Markdown via `termimad` before the
//! per-line cyan-bar prefix is applied.

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
fn purple() -> String { rgb(191, 64, 255) }
fn orange() -> String { rgb(255, 95, 31) }
fn red() -> String    { rgb(255, 50, 80) }
fn slate() -> String  { rgb(140, 150, 170) }

const W: usize = 70;

const ART: &str = " ███████╗██╗  ██╗ █████╗ ██████╗  ██████╗██╗  ██╗
 ██╔════╝╚██╗██╔╝██╔══██╗██╔══██╗██╔════╝██║  ██║
 █████╗   ╚███╔╝ ███████║██████╔╝██║     ███████║
 ██╔══╝   ██╔██╗ ██╔══██║██╔══██╗██║     ██╔══██║
 ███████╗██╔╝ ██╗██║  ██║██║  ██║╚██████╗██║  ██║
 ╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝╚═╝  ╚═╝";

pub fn banner(provider: &str, model: &str) {
    let p = pink();
    let c = cyan();
    let l = lime();
    let s = slate();
    eprintln!();
    for line in ART.lines() {
        eprintln!("{p}{BOLD}{line}{RESET}");
    }
    eprintln!(" {s}{ITAL}a delegate driving ral under a grant{RESET}");
    eprintln!(
        "{s}provider {RESET}{c}{BOLD}{provider}{RESET}    {s}model {RESET}{l}{BOLD}{model}{RESET}"
    );
    eprintln!("{}{}{RESET}", purple(), "━".repeat(W));
}

pub fn turn(n: usize) {
    let p = purple();
    let s = slate();
    eprintln!();
    eprintln!(
        "{p}◆ turn {BOLD}{n:02}{RESET} {s}{}{RESET}",
        "─".repeat(W.saturating_sub(10))
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

    let rendered = skin.text(text, Some(W - 2)).to_string();
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
/// Cache fields are shown only when non-zero (Anthropic only).
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
