//! Terminal capability snapshot.
//!
//! [`InteractiveMode`] resolves `RAL_INTERACTIVE_MODE` once at startup and
//! [`TerminalState`] caches the shell's entry-time isatty results plus the
//! ANSI / NO_COLOR / tmux / asciinema / CI bits and a small set of opt-in
//! capability flags (truecolor, OSC 8 hyperlinks, OSC 52 clipboard write,
//! bracketed paste).  Everything here is a snapshot — nothing re-queries
//! the OS mid-session.
//!
//! The capability flags follow the doc principle "probe, enable
//! opportunistically, and degrade cleanly to a keyboard-only monochrome
//! line mode": each flag has a pure probe over an explicit
//! [`TerminalEnv`] record so tests can drive them as data, and a public
//! `ui_*_ok` predicate that mixes the raw probe with the active mode and
//! NO_COLOR policy.
//!
//! Windows console / VTP detection ([`is_console`],
//! [`enable_virtual_terminal_processing`], and the `STD_*_HANDLE`
//! constants) lives at the bottom of this file.  `GetConsoleMode`
//! succeeds only on real console handles, making it a reliable
//! `isatty` substitute; `ENABLE_VIRTUAL_TERMINAL_PROCESSING` must be set
//! before any ANSI output because bundled uutils (uu_ls etc.) emit
//! escape codes but rely on the host process to have switched the
//! console into VTP mode first.
use serde::{Serialize, Deserialize};
/// Operating mode for the interactive frontend, resolved from
/// `RAL_INTERACTIVE_MODE` at shell startup.
///
/// `Auto` is the default: capability bits drive feature gating.
/// `Minimal` forces every terminal round-trip and every ANSI emission off.
/// `Full` forces ANSI on even when capability detection says otherwise
/// (useful when piping ral into a wrapper that understands ANSI).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractiveMode {
    #[default]
    Auto,
    Minimal,
    Full,
}

impl InteractiveMode {
    /// Parse the `RAL_INTERACTIVE_MODE` value.  Unknown values fall back to
    /// `Auto` and set `warn` so the caller can emit a one-time diagnostic.
    pub fn parse(raw: Option<&str>) -> (Self, Option<String>) {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("") | Some("auto") => (Self::Auto, None),
            Some("minimal") | Some("dumb") | Some("plain") => (Self::Minimal, None),
            Some("full") => (Self::Full, None),
            Some(other) => (
                Self::Auto,
                Some(format!(
                    "unknown RAL_INTERACTIVE_MODE '{other}', using auto"
                )),
            ),
        }
    }

    /// True when the mode suppresses all terminal output, round-trips, and ANSI.
    pub fn is_minimal(self) -> bool {
        matches!(self, Self::Minimal)
    }
}

/// Cached terminal capability snapshot, taken once at shell start.
///
/// `startup_stdin_tty` / `startup_stdout_tty` / `startup_stderr_tty` are the
/// raw `isatty(3)` results for fds 0/1/2 *at process entry*.  They are the
/// right oracle for "did the user launch us interactively?" and for any
/// fall-through code path that reads the inherited fd 0/1/2 directly; they
/// are the wrong oracle for "is fd N a tty *right now*?", since `<file` and
/// `>file` redirects can replace the underlying fd transiently.  Code that
/// asks a current-state question should route through the `Source`/`Sink`
/// abstractions instead of consulting these fields.
///
/// `startup_foreground` records whether ral's process group *owned* the
/// controlling terminal's foreground at process entry.  It is no longer
/// consulted as a per-handoff authority — that role moved to the session's
/// [`TerminalLease`](crate::process::TerminalLease).  It survives as the
/// lease's *mint condition*: core mints the session lease iff this is true at
/// construction (see `TerminalLease::mint_at_startup`).  True for an
/// interactive REPL and a non-interactive script launched at a terminal — both
/// own the foreground and so get a lease; an exarch tool-eval, a piped
/// `ral -c`, or a backgrounded `ral … &` does not.
///
/// The remaining fields record whether the terminal is known to accept ANSI
/// escape sequences and which "hostile but common" environment we are running
/// inside.  Population happens once via `TerminalState::probe_with_mode`;
/// nothing re-queries the OS mid-session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalState {
    pub startup_stdin_tty: bool,
    pub startup_stdout_tty: bool,
    pub startup_stderr_tty: bool,
    /// ral's process group owned the controlling terminal's foreground at
    /// process entry.  False when stdin is not a tty.  See the type doc.
    pub startup_foreground: bool,
    /// `true` when stdout is a tty *and* TERM/platform checks say ANSI works.
    pub supports_ansi: bool,
    /// `NO_COLOR` is set in the environment.
    pub no_color: bool,
    /// Running inside a tmux session.
    pub is_tmux: bool,
    /// Running under asciinema recording.
    pub is_asciinema: bool,
    /// Running in a CI environment (GitHub Actions, GitLab CI, etc.).
    pub is_ci: bool,
    /// 24-bit color advertised by `COLORTERM=truecolor` / `COLORTERM=24bit`.
    pub truecolor: bool,
    /// OSC 8 hyperlinks recognised by the host terminal.  Probed via
    /// `TERM_PROGRAM`, `KITTY_WINDOW_ID`, `WT_SESSION`, `VTE_VERSION ≥ 5000`,
    /// and a small `TERM=` allowlist.
    pub hyperlinks: bool,
    /// OSC 52 clipboard *write* is expected to land in the system clipboard.
    /// Read is intentionally not probed: many terminals gate it behind a
    /// permission prompt and the safer surface is write-only.
    pub clipboard_write: bool,
    /// Bracketed-paste mode is supported.  Universal in modern ANSI
    /// terminals; we use `supports_ansi` as a sufficient proxy without
    /// emitting a round-trip query.
    pub bracketed_paste: bool,
    /// Resolved from `RAL_INTERACTIVE_MODE`.
    pub mode: InteractiveMode,
}

impl TerminalState {
    /// Back-compat entry point: probe with `InteractiveMode::Auto`.
    pub fn probe() -> Self {
        Self::probe_with_mode(InteractiveMode::Auto)
    }

    /// Resolve `RAL_INTERACTIVE_MODE` from the environment and probe in that
    /// mode.  Returns the resolved mode, the terminal state, and an optional
    /// warning for an unrecognised mode value (the caller decides whether to
    /// surface it).  The env-var name and its parsing live here, beside the
    /// type that defines the modes, rather than being respelled per frontend.
    #[allow(clippy::disallowed_methods)] // mode selector, not a basedir
    pub fn probe_from_env() -> (InteractiveMode, Self, Option<String>) {
        let raw = std::env::var("RAL_INTERACTIVE_MODE").ok();
        let (mode, warn) = InteractiveMode::parse(raw.as_deref());
        (mode, Self::probe_with_mode(mode), warn)
    }

    /// Query the OS and environment for the current terminal state.
    // TMUX / ASCIINEMA_REC are presence probes, not basedirs.
    #[allow(clippy::disallowed_methods)]
    pub fn probe_with_mode(mode: InteractiveMode) -> Self {
        let (startup_stdin_tty, startup_stdout_tty, startup_stderr_tty) = probe_isatty();
        let startup_foreground = probe_foreground(startup_stdin_tty);
        let env = TerminalEnv::from_process();

        let supports_ansi = ansi_supported(mode, startup_stdout_tty);
        let modern_osc = env.recognises_modern_osc();

        TerminalState {
            startup_stdin_tty,
            startup_stdout_tty,
            startup_stderr_tty,
            startup_foreground,
            supports_ansi,
            no_color: anstyle_query::no_color(),
            is_tmux: std::env::var_os("TMUX").is_some(),
            is_asciinema: std::env::var_os("ASCIINEMA_REC").is_some(),
            is_ci: anstyle_query::is_ci(),
            truecolor: env.advertises_truecolor(),
            hyperlinks: modern_osc,
            clipboard_write: modern_osc,
            bracketed_paste: supports_ansi,
            mode,
        }
    }

    // ── Policy predicates ─────────────────────────────────────────────────
    //
    // Each `ui_*_ok` is "raw capability bit ∧ user/mode policy".  Keeping
    // the bits and the policy separate means a plugin that wants to
    // override policy (e.g. force OSC 8 emission while debugging) can read
    // the raw field; everyday code uses the predicate.

    /// UI may emit styling.  False under NO_COLOR, TERM=dumb, non-tty, or
    /// `RAL_INTERACTIVE_MODE=minimal`.
    pub fn ui_ansi_ok(&self) -> bool {
        !self.mode.is_minimal() && self.supports_ansi && !self.no_color
    }

    /// Terminal round-trip queries (CPR, DA, OSC) are appropriate.  False on
    /// non-tty stdout or in minimal mode.
    pub fn ui_round_trips_ok(&self) -> bool {
        self.startup_stdout_tty && !self.mode.is_minimal()
    }

    /// Terminal title may be set via OSC 0/2 sequences.
    pub fn ui_title_ok(&self) -> bool {
        self.ui_round_trips_ok()
    }

    /// 24-bit foreground/background colors may be emitted.  Subsumes
    /// `ui_ansi_ok`; NO_COLOR turns this off too.
    pub fn ui_truecolor_ok(&self) -> bool {
        self.ui_ansi_ok() && self.truecolor
    }

    /// OSC 8 hyperlinks may be emitted.  NO_COLOR does not block hyperlinks
    /// — they are structural, not color — but minimal mode and non-tty
    /// stdout both do.
    pub fn ui_hyperlinks_ok(&self) -> bool {
        self.ui_round_trips_ok() && self.hyperlinks
    }

    /// OSC 52 clipboard writes may be emitted.  Write-only; reads are not
    /// surfaced because the permission-prompt landscape is too uneven.
    pub fn ui_clipboard_write_ok(&self) -> bool {
        self.ui_round_trips_ok() && self.clipboard_write
    }

    /// Bracketed-paste mode may be enabled by the line editor.
    pub fn ui_bracketed_paste_ok(&self) -> bool {
        self.ui_round_trips_ok() && self.bracketed_paste
    }

    /// Diagnostics (stderr) may emit ANSI.  Independent of `ui_ansi_ok`
    /// because stderr may be a tty while stdout is piped to a pager; we still
    /// want colored errors in that case.  False under NO_COLOR, TERM=dumb on
    /// Auto, non-tty stderr, or minimal mode.
    pub fn stderr_ansi_ok(&self) -> bool {
        !self.mode.is_minimal()
            && !self.no_color
            && self.startup_stderr_tty
            && (matches!(self.mode, InteractiveMode::Full)
                || anstyle_query::term_supports_ansi_color())
    }

    /// Project the terminal snapshot into the user-visible `$TERMINAL`
    /// map exposed to RC files and plugins.  The shape is stable: ral
    /// scripts pattern-match on these keys, so adding a key is OK but
    /// renaming or removing one is a breaking change.
    pub fn to_value(&self) -> crate::types::Value {
        use crate::types::Value;
        let mode = match self.mode {
            InteractiveMode::Auto => "auto",
            InteractiveMode::Minimal => "minimal",
            InteractiveMode::Full => "full",
        };
        Value::map(vec![
            ("stdin_tty".into(), Value::Bool(self.startup_stdin_tty)),
            ("stdout_tty".into(), Value::Bool(self.startup_stdout_tty)),
            ("stderr_tty".into(), Value::Bool(self.startup_stderr_tty)),
            ("supports_ansi".into(), Value::Bool(self.supports_ansi)),
            ("no_color".into(), Value::Bool(self.no_color)),
            ("is_tmux".into(), Value::Bool(self.is_tmux)),
            ("is_asciinema".into(), Value::Bool(self.is_asciinema)),
            ("is_ci".into(), Value::Bool(self.is_ci)),
            ("truecolor".into(), Value::Bool(self.truecolor)),
            ("hyperlinks".into(), Value::Bool(self.hyperlinks)),
            ("clipboard_write".into(), Value::Bool(self.clipboard_write)),
            ("bracketed_paste".into(), Value::Bool(self.bracketed_paste)),
            ("ui_ansi_ok".into(), Value::Bool(self.ui_ansi_ok())),
            (
                "ui_truecolor_ok".into(),
                Value::Bool(self.ui_truecolor_ok()),
            ),
            (
                "ui_hyperlinks_ok".into(),
                Value::Bool(self.ui_hyperlinks_ok()),
            ),
            (
                "ui_clipboard_write_ok".into(),
                Value::Bool(self.ui_clipboard_write_ok()),
            ),
            (
                "ui_bracketed_paste_ok".into(),
                Value::Bool(self.ui_bracketed_paste_ok()),
            ),
            ("mode".into(), Value::String(mode.into())),
        ])
    }
}

// ── isatty + ANSI gating: small named helpers ─────────────────────────────

/// `(stdin, stdout, stderr)` tty membership at process entry.
fn probe_isatty() -> (bool, bool, bool) {
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        (
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        )
    }
    #[cfg(windows)]
    {
        (
            is_console(STD_INPUT_HANDLE),
            is_console(STD_OUTPUT_HANDLE),
            is_console(STD_ERROR_HANDLE),
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        (false, false, false)
    }
}

/// Whether ral's process group owns the controlling terminal's foreground.
///
/// Only meaningful when stdin is a tty; `false` otherwise.  On Unix this is
/// `tcgetpgrp(stdin) == getpgrp()` — true for an interactive REPL and for a
/// script launched in the foreground, false for a backgrounded `ral … &` or
/// a tty-less eval.  Windows consoles are shared between attached processes
/// and have no `tcsetpgrp`, so a console-attached shell is taken to own the
/// foreground; the value only feeds capability decisions there.
fn probe_foreground(stdin_tty: bool) -> bool {
    if !stdin_tty {
        return false;
    }
    #[cfg(unix)]
    {
        let fg = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        let me = unsafe { libc::getpgrp() };
        fg >= 0 && fg == me
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `true` when ANSI styling is acceptable on stdout under the given mode.
///
/// `Full` forces ANSI on even with a piped stdout; `Minimal` forces it
/// off; `Auto` defers to anstyle-query + isatty.
fn ansi_supported(mode: InteractiveMode, stdout_tty: bool) -> bool {
    match mode {
        InteractiveMode::Full => true,
        InteractiveMode::Minimal => false,
        InteractiveMode::Auto => stdout_tty && anstyle_query::term_supports_ansi_color(),
    }
}

// ── TerminalEnv: owned env snapshot for capability probes ─────────────────

/// The env values the capability probes consult, captured once so the
/// probes themselves can be pure procedures on data.  Tests construct
/// literal values; production calls [`TerminalEnv::from_process`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TerminalEnv {
    term: Option<String>,
    term_program: Option<String>,
    colorterm: Option<String>,
    vte_version: Option<String>,
    kitty_window_id: bool,
    wt_session: bool,
}

impl TerminalEnv {
    /// Read every env var the probes care about from the process
    /// environment.  Bool flags collapse "present" into `true`.
    // KITTY_WINDOW_ID / WT_SESSION are presence probes, not basedirs.
    #[allow(clippy::disallowed_methods)]
    fn from_process() -> Self {
        TerminalEnv {
            term: std::env::var("TERM").ok(),
            term_program: std::env::var("TERM_PROGRAM").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            vte_version: std::env::var("VTE_VERSION").ok(),
            kitty_window_id: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            wt_session: std::env::var_os("WT_SESSION").is_some(),
        }
    }

    /// `COLORTERM` advertises 24-bit color.  This is the de-facto signal
    /// every major terminal honours.
    fn advertises_truecolor(&self) -> bool {
        matches!(self.colorterm.as_deref(), Some("truecolor") | Some("24bit"))
    }

    /// The host terminal is one of the known cohort that recognises both
    /// OSC 8 hyperlinks and OSC 52 clipboard writes.  Any one source of
    /// evidence suffices:
    ///
    /// * `TERM_PROGRAM` ∈ {iTerm.app, WezTerm, vscode, ghostty}
    /// * `TERM` ∈ {xterm-kitty, foot, xterm-ghostty}
    /// * `KITTY_WINDOW_ID` is set
    /// * `WT_SESSION` is set (Windows Terminal)
    /// * `VTE_VERSION` ≥ 5000 (VTE 0.50+: gnome-terminal, tilix, …)
    fn recognises_modern_osc(&self) -> bool {
        self.term_program_is_modern()
            || self.term_is_modern()
            || self.vte_version_at_least(5000)
            || self.kitty_window_id
            || self.wt_session
    }

    fn term_program_is_modern(&self) -> bool {
        matches!(
            self.term_program.as_deref(),
            Some("iTerm.app" | "WezTerm" | "vscode" | "ghostty"),
        )
    }

    fn term_is_modern(&self) -> bool {
        matches!(
            self.term.as_deref(),
            Some("xterm-kitty" | "foot" | "xterm-ghostty"),
        )
    }

    fn vte_version_at_least(&self, minimum: u32) -> bool {
        self.vte_version
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
            .is_some_and(|n| n >= minimum)
    }
}

// ── Windows console: TTY detection and VTP setup ──────────────────────────

#[cfg(windows)]
pub(crate) use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleMode,
};

/// Returns true when the given Win32 standard-handle ID is attached to a
/// console (not a pipe, file, or NUL).  Used as `isatty` on Windows.
#[cfg(windows)]
pub(crate) fn is_console(std_handle: u32) -> bool {
    let h = unsafe { GetStdHandle(std_handle) };
    let mut mode: u32 = 0;
    unsafe { GetConsoleMode(h, &mut mode) != 0 }
}

/// Enable ANSI virtual-terminal processing on the stdout and stderr console
/// handles.  Must be called once at process startup.  A no-op when a handle
/// is redirected to a pipe or file (`GetConsoleMode` will fail on those).
#[cfg(windows)]
pub fn enable_virtual_terminal_processing() {
    const ENABLE_VTP: u32 = 0x0004;
    for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = unsafe { GetStdHandle(id) };
        let mut mode: u32 = 0;
        if unsafe { GetConsoleMode(h, &mut mode) } != 0 {
            unsafe {
                SetConsoleMode(h, mode | ENABLE_VTP);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_mode_parse() {
        let parse = |s| InteractiveMode::parse(s).0;
        assert_eq!(parse(None), InteractiveMode::Auto);
        assert_eq!(parse(Some("")), InteractiveMode::Auto);
        assert_eq!(parse(Some("auto")), InteractiveMode::Auto);
        assert_eq!(parse(Some("AUTO")), InteractiveMode::Auto);
        assert_eq!(parse(Some("  full ")), InteractiveMode::Full);
        assert_eq!(parse(Some("minimal")), InteractiveMode::Minimal);
        assert_eq!(parse(Some("dumb")), InteractiveMode::Minimal);
        let (mode, warn) = InteractiveMode::parse(Some("bogus"));
        assert_eq!(mode, InteractiveMode::Auto);
        assert!(warn.is_some());
    }

    /// Compact constructor for predicate tests.  Capability flags mirror
    /// the production probe: extras live under `supports_ansi` on a tty.
    fn make_state(
        mode: InteractiveMode,
        supports_ansi: bool,
        no_color: bool,
        stdout_tty: bool,
    ) -> TerminalState {
        let extras_allowed = !mode.is_minimal() && supports_ansi;
        TerminalState {
            startup_stdin_tty: stdout_tty,
            startup_stdout_tty: stdout_tty,
            startup_stderr_tty: stdout_tty,
            startup_foreground: stdout_tty,
            supports_ansi,
            no_color,
            is_tmux: false,
            is_asciinema: false,
            is_ci: false,
            truecolor: extras_allowed,
            hyperlinks: extras_allowed,
            clipboard_write: extras_allowed,
            bracketed_paste: extras_allowed,
            mode,
        }
    }

    #[test]
    fn ui_ansi_ok_gates() {
        assert!(make_state(InteractiveMode::Auto, true, false, true).ui_ansi_ok());
        assert!(!make_state(InteractiveMode::Auto, true, true, true).ui_ansi_ok());
        assert!(!make_state(InteractiveMode::Auto, false, false, true).ui_ansi_ok());
        assert!(!make_state(InteractiveMode::Minimal, true, false, true).ui_ansi_ok());
        // Full mode still respects NO_COLOR (user intent overrides force).
        assert!(!make_state(InteractiveMode::Full, true, true, true).ui_ansi_ok());
    }

    #[test]
    fn ui_round_trips_ok_gates() {
        assert!(make_state(InteractiveMode::Auto, true, false, true).ui_round_trips_ok());
        assert!(!make_state(InteractiveMode::Auto, true, false, false).ui_round_trips_ok());
        assert!(!make_state(InteractiveMode::Minimal, true, false, true).ui_round_trips_ok());
    }

    #[test]
    fn stderr_ansi_ok_gates() {
        assert!(!make_state(InteractiveMode::Auto, true, false, false).stderr_ansi_ok());
        assert!(!make_state(InteractiveMode::Auto, true, true, true).stderr_ansi_ok());
        assert!(!make_state(InteractiveMode::Minimal, true, false, true).stderr_ansi_ok());
        // Full on a tty with no NO_COLOR → on, regardless of TERM checks.
        assert!(make_state(InteractiveMode::Full, false, false, true).stderr_ansi_ok());
    }

    // ── New capability probes ─────────────────────────────────────────────

    /// Build a `TerminalEnv` from named pairs.  `None` for "unset".
    fn env(
        term: Option<&str>,
        term_program: Option<&str>,
        colorterm: Option<&str>,
        vte_version: Option<&str>,
        kitty_window_id: bool,
        wt_session: bool,
    ) -> TerminalEnv {
        TerminalEnv {
            term: term.map(String::from),
            term_program: term_program.map(String::from),
            colorterm: colorterm.map(String::from),
            vte_version: vte_version.map(String::from),
            kitty_window_id,
            wt_session,
        }
    }

    #[test]
    fn truecolor_follows_colorterm() {
        assert!(!env(None, None, None, None, false, false).advertises_truecolor());
        assert!(env(None, None, Some("truecolor"), None, false, false).advertises_truecolor());
        assert!(env(None, None, Some("24bit"), None, false, false).advertises_truecolor());
        assert!(!env(None, None, Some("ansi"), None, false, false).advertises_truecolor());
    }

    #[test]
    fn modern_osc_recognised_via_term_program() {
        for tp in ["iTerm.app", "WezTerm", "vscode", "ghostty"] {
            assert!(
                env(None, Some(tp), None, None, false, false).recognises_modern_osc(),
                "expected modern OSC for TERM_PROGRAM={tp}",
            );
        }
        assert!(
            !env(None, Some("Apple_Terminal"), None, None, false, false).recognises_modern_osc()
        );
    }

    #[test]
    fn modern_osc_recognised_via_term() {
        for t in ["xterm-kitty", "foot", "xterm-ghostty"] {
            assert!(
                env(Some(t), None, None, None, false, false).recognises_modern_osc(),
                "expected modern OSC for TERM={t}",
            );
        }
        assert!(
            !env(Some("xterm-256color"), None, None, None, false, false).recognises_modern_osc()
        );
    }

    #[test]
    fn modern_osc_recognised_via_kitty_or_wt() {
        assert!(env(None, None, None, None, true, false).recognises_modern_osc());
        assert!(env(None, None, None, None, false, true).recognises_modern_osc());
    }

    #[test]
    fn modern_osc_recognised_via_vte_version() {
        assert!(env(None, None, None, Some("5000"), false, false).recognises_modern_osc());
        assert!(env(None, None, None, Some("7600"), false, false).recognises_modern_osc());
        assert!(!env(None, None, None, Some("4900"), false, false).recognises_modern_osc());
        assert!(!env(None, None, None, Some("garbage"), false, false).recognises_modern_osc());
    }

    #[test]
    fn modern_osc_not_recognised_when_env_blank() {
        assert!(!env(None, None, None, None, false, false).recognises_modern_osc());
    }

    #[test]
    fn ui_truecolor_ok_requires_ansi_and_no_no_color() {
        let mut s = make_state(InteractiveMode::Auto, true, false, true);
        s.truecolor = true;
        assert!(s.ui_truecolor_ok());

        s.no_color = true;
        assert!(!s.ui_truecolor_ok(), "NO_COLOR disables truecolor");

        s.no_color = false;
        s.supports_ansi = false;
        assert!(!s.ui_truecolor_ok(), "no ANSI disables truecolor");

        s.supports_ansi = true;
        s.truecolor = false;
        assert!(!s.ui_truecolor_ok(), "raw bit off disables truecolor");
    }

    #[test]
    fn ui_hyperlinks_ok_ignores_no_color_but_needs_tty() {
        let mut s = make_state(InteractiveMode::Auto, true, true, true);
        s.hyperlinks = true;
        // NO_COLOR is set above; hyperlinks are structural, not color, so still OK.
        assert!(s.ui_hyperlinks_ok());

        s.startup_stdout_tty = false;
        assert!(!s.ui_hyperlinks_ok(), "non-tty disables hyperlinks");

        s.startup_stdout_tty = true;
        s.mode = InteractiveMode::Minimal;
        assert!(!s.ui_hyperlinks_ok(), "minimal mode disables hyperlinks");
    }

    #[test]
    fn ui_clipboard_write_ok_tracks_round_trips() {
        let mut s = make_state(InteractiveMode::Auto, true, false, true);
        s.clipboard_write = true;
        assert!(s.ui_clipboard_write_ok());
        s.mode = InteractiveMode::Minimal;
        assert!(!s.ui_clipboard_write_ok());
    }

    #[test]
    fn ui_bracketed_paste_ok_tracks_round_trips() {
        let mut s = make_state(InteractiveMode::Auto, true, false, true);
        s.bracketed_paste = true;
        assert!(s.ui_bracketed_paste_ok());
        s.startup_stdout_tty = false;
        assert!(!s.ui_bracketed_paste_ok());
    }

    #[test]
    fn to_value_carries_every_field() {
        let state = TerminalState {
            startup_stdin_tty: true,
            startup_stdout_tty: false,
            startup_stderr_tty: true,
            startup_foreground: true,
            supports_ansi: true,
            no_color: false,
            is_tmux: true,
            is_asciinema: false,
            is_ci: false,
            truecolor: true,
            hyperlinks: true,
            clipboard_write: false,
            bracketed_paste: true,
            mode: InteractiveMode::Full,
        };
        let crate::types::Value::Map(m) = state.to_value() else {
            panic!("expected map");
        };
        let get = |k: &str| m.iter().find(|(kk, _)| *kk == k).map(|(_, v)| v.clone());
        let expect_bool = |k: &str, b: bool| {
            assert_eq!(get(k), Some(crate::types::Value::Bool(b)), "key {k}");
        };
        expect_bool("stdin_tty", true);
        expect_bool("stdout_tty", false);
        expect_bool("stderr_tty", true);
        expect_bool("supports_ansi", true);
        expect_bool("no_color", false);
        expect_bool("is_tmux", true);
        expect_bool("is_asciinema", false);
        expect_bool("is_ci", false);
        expect_bool("truecolor", true);
        expect_bool("hyperlinks", true);
        expect_bool("clipboard_write", false);
        expect_bool("bracketed_paste", true);
        expect_bool("ui_ansi_ok", state.ui_ansi_ok());
        expect_bool("ui_truecolor_ok", state.ui_truecolor_ok());
        expect_bool("ui_hyperlinks_ok", state.ui_hyperlinks_ok());
        expect_bool("ui_clipboard_write_ok", state.ui_clipboard_write_ok());
        expect_bool("ui_bracketed_paste_ok", state.ui_bracketed_paste_ok());
        assert_eq!(
            get("mode"),
            Some(crate::types::Value::String("full".into())),
        );
    }
}
