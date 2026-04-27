//! Exarch runtime: sandbox bootstrap, shell construction, the per-task
//! turn loop.  Everything here is single-threaded and synchronous; the
//! REPL above just calls `run_task` once per user line.

use crate::api::{Provider, Step, StepOut, ToolCall, Usage};
use crate::{eval, grant, ui};
use ral_core::io::TerminalState;
use ral_core::{Shell, builtins, diagnostic};
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io;
use std::path::PathBuf;

const MAX_TURNS: usize = 40;

/// Maximum bytes of a single tool result kept in the conversation
/// history; output longer than this is replaced with a head+tail
/// summary plus a pointer to the full output on disk.  The user still
/// sees the full text on the terminal — this only affects what the
/// model receives on subsequent turns.
const MAX_TOOL_RESULT: usize = 16 * 1024;

/// Soft cap on history bytes before the exarch compacts the
/// conversation between tasks.  ≈ 1 token / 3–4 bytes, so 200 KB
/// ≈ 50–65k tokens — beyond this the per-turn replay cost dominates.
const COMPACT_THRESHOLD: usize = 200 * 1024;

/// Point `RAL_SANDBOX_SELF_BIN` at our own executable so ral's
/// `eval_grant` re-execs us as a sandbox child.
pub fn enable_os_sandbox() {
    if std::env::var_os("RAL_SANDBOX_SELF_BIN").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        // SAFETY: set before any thread that reads env is spawned.
        unsafe { std::env::set_var("RAL_SANDBOX_SELF_BIN", &exe) };
    }
}

/// If we were re-execed as a sandbox child, ral's `early_init` handles
/// the IPC block and tells us to exit.  Otherwise it returns `None`.
pub fn sandbox_dispatch_or_continue() -> Option<std::process::ExitCode> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match ral_core::sandbox::early_init(&argv) {
        Ok((_, exit)) => exit,
        Err(e) => {
            eprintln!("exarch: sandbox init: {e}");
            Some(std::process::ExitCode::from(1))
        }
    }
}

pub fn boot_shell() -> Shell {
    ral_core::signal::install_handlers();
    let raw = std::env::var("RAL_INTERACTIVE_MODE").ok();
    let (mode, _) = ral_core::io::InteractiveMode::parse(raw.as_deref());
    let terminal = TerminalState::probe_with_mode(mode);
    diagnostic::set_terminal(&terminal);
    let mut shell = Shell::new(terminal);
    eval::seed_default_env(&mut shell);
    builtins::register(&mut shell, eval::baked_prelude_comp());
    ral_core::builtins::misc::register_prelude_type_hints(eval::baked_prelude_schemes());
    shell
}

/// Drive `provider` for one user message until it stops calling tools.
pub fn run_task(
    provider: &mut Provider,
    shell: &mut Shell,
    spec: &grant::GrantSpec,
    spill: &Spill,
    total: &mut Usage,
    prompt: String,
) -> Result<Usage, String> {
    let mut task = Usage::default();
    let mut input = Step::User(prompt);
    for n in 1..=MAX_TURNS {
        ui::turn(n);
        let StepOut { text, tool_calls, done, usage } = provider.step(input)?;
        task += usage;
        *total += usage;
        if !text.is_empty() {
            ui::assistant_text(&text);
        }
        if tool_calls.is_empty() {
            return Ok(task);
        }
        let mut results = Vec::with_capacity(tool_calls.len());
        for ToolCall { id, cmd, audit } in tool_calls {
            ui::tool_call(&cmd, audit);
            match eval::run_shell(shell, spec, &cmd, audit) {
                eval::Outcome::Ran(r) => {
                    // Audit JSON is large; the user sees a one-line node-
                    // count summary while the model gets the (capped) JSON.
                    ui::tool_result(&format_for_display(&r));
                    results.push((id, format_for_history(&r, spill)));
                }
                eval::Outcome::Static(s) => {
                    ui::tool_result(&s);
                    results.push((id, opaque_truncate(s, spill)));
                }
            }
        }
        if done {
            return Ok(task);
        }
        input = Step::ToolResults(results);
    }
    ui::error("max turns reached for this task");
    Ok(task)
}

/// Per-session spill directory under `/tmp`, owning the dir and
/// removing it on drop.  Oversized tool outputs are written here so the
/// model can `head`, `tail`, or `rg` the full text via the shell when
/// the head+tail summary in history is not enough.  `/tmp` is in the
/// default fs grant, so the model can read these files without prompt.
pub struct Spill {
    dir: PathBuf,
}

impl Spill {
    pub fn new() -> io::Result<Self> {
        let dir = PathBuf::from(format!("/tmp/exarch-{}", std::process::id()));
        // A previous process with this pid (since reused by the OS) may
        // have left a dir behind — wipe it before reusing the name.
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Test-only constructor that places the spill dir at a caller-
    /// chosen path so concurrent tests don't race on a shared name.
    #[cfg(test)]
    fn with_dir(dir: PathBuf) -> io::Result<Self> {
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Write `bytes` under a content-hashed name and return the path.
    /// Identical bytes hash to the same filename, so repeated outputs
    /// are deduplicated for free.
    fn write(&self, bytes: &[u8]) -> io::Result<PathBuf> {
        let mut h = DefaultHasher::new();
        h.write(bytes);
        let path = self.dir.join(format!("{:016x}.out", h.finish()));
        if !path.exists() {
            fs::write(&path, bytes)?;
        }
        Ok(path)
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Per-section caps applied independently so a chatty stdout can't
/// crowd out a short stderr or audit trace.  Sum + overhead stays
/// within `MAX_TOOL_RESULT`.
const STDOUT_CAP: usize = 5 * 1024;
const STDERR_CAP: usize = 5 * 1024;
const VALUE_CAP: usize = 1 * 1024;
const AUDIT_CAP: usize = 4 * 1024;

/// Render `r` with the section layout `eval` historically emitted.
/// `capped` enables per-section caps; `audit_summary` swaps the audit
/// JSON for a one-line node-count for terminal display.  Returns the
/// rendered string and whether anything was elided.
fn render(r: &eval::ToolResult, capped: bool, audit_summary: bool) -> (String, bool) {
    use std::fmt::Write;
    let cap = |c: usize| capped.then_some(c);
    let mut out = String::new();
    let mut cut = false;
    if !r.stdout.is_empty() {
        cut |= push(&mut out, "STDOUT:\n", &String::from_utf8_lossy(&r.stdout), cap(STDOUT_CAP));
    }
    if !r.stderr.is_empty() {
        cut |= push(&mut out, "STDERR:\n", &String::from_utf8_lossy(&r.stderr), cap(STDERR_CAP));
    }
    if let Some(v) = &r.value {
        cut |= push(&mut out, "VALUE:\n", v, cap(VALUE_CAP));
        out.push('\n');
    }
    let _ = write!(out, "\nEXIT: {}", r.exit);
    if let Some(json) = &r.audit {
        if audit_summary {
            let nodes = json.matches("\"cmd\":").count();
            let _ = write!(out, "\n[+ audit tree, {nodes} node(s)]");
        } else {
            cut |= push(&mut out, "AUDIT:\n", json, cap(AUDIT_CAP));
        }
    }
    (out, cut)
}

fn format_for_display(r: &eval::ToolResult) -> String {
    render(r, false, true).0
}

/// Cap each section to fit in `MAX_TOOL_RESULT`.  If anything was
/// elided, spill the full original to disk and append a pointer line.
fn format_for_history(r: &eval::ToolResult, spill: &Spill) -> String {
    let (full, _) = render(r, false, false);
    if full.len() <= MAX_TOOL_RESULT {
        return full;
    }
    let (capped, any_cut) = render(r, true, false);
    if !any_cut {
        return full; // unreachable given current caps, but be defensive
    }
    match spill.write(full.as_bytes()).ok() {
        Some(p) => format!("{capped}\n[full output spilled to {} (use head/tail/rg)]", p.display()),
        None => capped,
    }
}

/// Append `label` + `body` to `out` with a leading '\n' if non-empty.
/// If `cap` is `Some`, the body is run through `head_tail`.  Returns
/// true iff bytes were elided.
fn push(out: &mut String, label: &str, body: &str, cap: Option<usize>) -> bool {
    if !out.is_empty() { out.push('\n'); }
    out.push_str(label);
    match cap.and_then(|c| head_tail(body, c, "")) {
        Some(s) => { out.push_str(&s); true }
        None => { out.push_str(body); false }
    }
}

/// Truncation for `Outcome::Static` blobs (parse / type errors) which
/// have no internal structure: the spill path goes inline in the marker.
fn opaque_truncate(s: String, spill: &Spill) -> String {
    if s.len() <= MAX_TOOL_RESULT {
        return s;
    }
    let extra = spill.write(s.as_bytes()).ok()
        .map(|p| format!("; full output at {} (use head/tail/rg)", p.display()))
        .unwrap_or_default();
    head_tail(&s, MAX_TOOL_RESULT, &extra).unwrap_or(s)
}

/// Head+tail digest.  Returns `None` if `s` fits in `cap`.  Otherwise
/// returns a digest with an `[elided N bytes{extra}]` marker.  Cuts
/// prefer a newline boundary in a small window, else a UTF-8 boundary.
fn head_tail(s: &str, cap: usize, extra: &str) -> Option<String> {
    if s.len() <= cap {
        return None;
    }
    let half = cap.saturating_sub(64 + extra.len()) / 2;
    let head_end = align_cut_back(s, half);
    let tail_start = align_cut_forward(s, s.len() - half);
    let omitted = tail_start - head_end;
    Some(format!(
        "{}\n... [elided {omitted} bytes{extra}] ...\n{}",
        &s[..head_end],
        &s[tail_start..],
    ))
}

/// Walk back from `idx` to a newline within a small window, falling
/// back to the nearest UTF-8 boundary at or before `idx`.
fn align_cut_back(s: &str, idx: usize) -> usize {
    const WINDOW: usize = 1024;
    let lo = idx.saturating_sub(WINDOW);
    if let Some(off) = s.as_bytes()[lo..idx].iter().rposition(|&b| b == b'\n') {
        return lo + off + 1;
    }
    let mut cut = idx;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// Walk forward from `idx` to one past a newline within a small
/// window, falling back to the nearest UTF-8 boundary at or after
/// `idx`.
fn align_cut_forward(s: &str, idx: usize) -> usize {
    const WINDOW: usize = 1024;
    let hi = (idx + WINDOW).min(s.len());
    if let Some(off) = s.as_bytes()[idx..hi].iter().position(|&b| b == b'\n') {
        return idx + off + 1;
    }
    let mut cut = idx;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    cut
}

/// If the conversation has grown past `COMPACT_THRESHOLD`, run a
/// summary turn and reset history.  Returns the cost charged for the
/// summary call so the caller can fold it into its running total.
pub fn maybe_compact(provider: &mut Provider, total: &mut Usage) {
    let bytes = provider.history_bytes();
    if bytes < COMPACT_THRESHOLD {
        return;
    }
    eprintln!(
        "\x1b[2m[compacting history: {} KB → summary]\x1b[0m",
        bytes / 1024,
    );
    match provider.compact() {
        Ok((inp, out, dollars)) => {
            *total += Usage { input: inp, output: out, cache_creation: 0, cache_read: 0, dollars };
            eprintln!(
                "\x1b[2m[compacted: now {} KB]\x1b[0m",
                provider.history_bytes() / 1024,
            );
        }
        Err(e) => ui::error(&format!("compact failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test uses its own dir so concurrent runs (cargo test threads)
    /// don't trample each other's spill files.
    fn fresh_spill(tag: &str) -> Spill {
        let dir =
            std::env::temp_dir().join(format!("exarch-test-{}-{tag}", std::process::id()));
        Spill::with_dir(dir).expect("spill dir")
    }

    fn tr(stdout: &str, stderr: &str, value: Option<&str>, exit: i32, audit: Option<&str>)
        -> eval::ToolResult
    {
        eval::ToolResult {
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            value: value.map(str::to_string),
            exit,
            audit: audit.map(str::to_string),
        }
    }

    #[test]
    fn head_tail_keeps_both_ends_aligned_to_newlines() {
        let head = "FIRST_LINE\n".repeat(2000);
        let tail = "LAST_LINE\n".repeat(2000);
        let input = format!("{head}{}{tail}", "X".repeat(50_000));
        let out = head_tail(&input, MAX_TOOL_RESULT, "").unwrap();
        assert!(out.contains("FIRST_LINE") && out.contains("LAST_LINE"));
        assert!(out.contains("\n... [elided") && out.contains("] ...\n"));
        assert!(!out.contains(&"X".repeat(1000)));
        assert!(out.len() <= MAX_TOOL_RESULT + 64);
    }

    #[test]
    fn head_tail_passes_short_input_through() {
        assert!(head_tail("short", 1024, "").is_none());
    }

    #[test]
    fn handles_utf8_at_cut_boundary() {
        let input = "λ".repeat(20_000);
        assert!(head_tail(&input, MAX_TOOL_RESULT, "").unwrap().contains("elided"));
    }

    #[test]
    fn render_matches_legacy_layout() {
        // Byte-for-byte equal to what eval.rs used to build by hand.
        let r = tr("abc\n", "err\n", Some("v"), 0, None);
        assert_eq!(render(&r, false, false).0, "STDOUT:\nabc\n\nSTDERR:\nerr\n\nVALUE:\nv\n\nEXIT: 0");
    }

    #[test]
    fn format_for_display_summarises_audit() {
        let r = tr("hi\n", "", None, 0, Some(r#"[{"cmd":"a"},{"cmd":"b"}]"#));
        let out = format_for_display(&r);
        assert!(out.ends_with("\n[+ audit tree, 2 node(s)]"));
        assert!(!out.contains("AUDIT:\n"));
    }

    #[test]
    fn small_result_passes_through_to_history() {
        let r = tr("hello\n", "", None, 0, None);
        assert_eq!(format_for_history(&r, &fresh_spill("small")), "STDOUT:\nhello\n\nEXIT: 0");
    }

    #[test]
    fn stderr_tail_survives_huge_stdout() {
        // The failure mode that motivated per-section caps: a chatty
        // stdout used to bury a short, important stderr.
        let stdout = "noise\n".repeat(20_000);
        let stderr = format!(
            "starting...\n{}\nERROR: division by zero at line 42\n",
            "warning: x\n".repeat(10),
        );
        let out = format_for_history(&tr(&stdout, &stderr, None, 1, None), &fresh_spill("tail"));
        assert!(out.contains("ERROR: division by zero at line 42"));
        assert!(out.contains("STDERR:\nstarting...\n"));
        assert!(out.contains("[full output spilled to "));
    }

    #[test]
    fn spill_writes_full_output_and_dedupes() {
        let spill = fresh_spill("spill");
        let r = tr(&"y".repeat(MAX_TOOL_RESULT * 2), "", None, 0, None);
        let out = format_for_history(&r, &spill);
        let path: PathBuf = out
            .split_once("[full output spilled to ").unwrap().1
            .split_whitespace().next().unwrap().into();
        assert_eq!(fs::read_to_string(&path).unwrap(), render(&r, false, false).0);
        // Same content → same path (content-hashed).
        assert_eq!(format_for_history(&r, &spill), out);
    }

    #[test]
    fn opaque_truncate_short_and_long() {
        let spill = fresh_spill("opaque");
        assert_eq!(opaque_truncate("hi".into(), &spill), "hi");
        let out = opaque_truncate("x".repeat(MAX_TOOL_RESULT * 2), &spill);
        assert!(out.contains("full output at ") && out.contains("head/tail/rg"));
    }

    #[test]
    fn spill_drop_removes_dir() {
        let dir = std::env::temp_dir()
            .join(format!("exarch-test-{}-drop", std::process::id()));
        {
            let spill = Spill::with_dir(dir.clone()).unwrap();
            spill.write(&"z".repeat(MAX_TOOL_RESULT * 2).into_bytes()).unwrap();
            assert!(dir.exists());
        }
        assert!(!dir.exists(), "drop should remove the spill dir");
    }
}
