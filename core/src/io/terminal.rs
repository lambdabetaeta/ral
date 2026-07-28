//! Terminal capabilities, probed once at shell start and never re-queried.
//!
//! Each capability is a pure probe over a `TerminalEnv` record, so tests can
//! drive it as data, paired with a `ui_*_ok` predicate that mixes the raw bit
//! with the active mode and `NO_COLOR` policy.
use serde::{Deserialize, Serialize};
/// Frontend operating mode, resolved from `RAL_INTERACTIVE_MODE` at startup.
///
/// `Auto` gates on the capability bits; `Minimal` forces every round-trip and
/// every ANSI emission off; `Full` forces ANSI on even for a piped stdout, for
/// a wrapper that understands the escapes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractiveMode {
    #[default]
    Auto,
    Minimal,
    Full,
}

impl InteractiveMode {
    /// Parse the `RAL_INTERACTIVE_MODE` value; an unknown one falls back to
    /// `Auto` and returns a warning for the caller to surface once.
    pub fn parse(raw: Option<&str>) -> (Self, Option<String>) {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("" | "auto") => (Self::Auto, None),
            Some("minimal" | "dumb" | "plain") => (Self::Minimal, None),
            Some("full") => (Self::Full, None),
            Some(other) => (
                Self::Auto,
                Some(format!(
                    "unknown RAL_INTERACTIVE_MODE '{other}', using auto"
                )),
            ),
        }
    }

    pub fn is_minimal(self) -> bool {
        matches!(self, Self::Minimal)
    }
}

/// Terminal capabilities as they stood at process entry.
///
/// The `startup_*_tty` bits answer "was ral launched at a terminal?", never "is
/// fd N a tty right now".  Consult one only when the matching `Source`/`Sink`
/// is `Terminal`, since that is exactly the case where the bytes still reach
/// the inherited fd; a `<file` or `>file` redirect parks elsewhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
// A flat record of independent facts, not a state machine; sub-structs would
// obscure rather than clarify.
#[allow(clippy::struct_excessive_bools)]
pub struct TerminalState {
    pub startup_stdin_tty: bool,
    pub startup_stdout_tty: bool,
    pub startup_stderr_tty: bool,
    /// ral's process group owned the controlling terminal's foreground, and so
    /// the mint condition for the session's
    /// [`TerminalLease`](crate::process::TerminalLease): an interactive REPL or
    /// a script launched at a terminal gets one, a piped `ral -c` or a
    /// backgrounded `ral … &` does not.  False when stdin is not a tty.
    pub startup_foreground: bool,
    /// stdout is a tty and TERM says ANSI works, or the mode is `Full`.
    pub supports_ansi: bool,
    /// TERM says stderr's terminal accepts ANSI.  Snapshotted here so
    /// `stderr_ansi_ok` never has to live-query.
    pub stderr_ansi_capable: bool,
    pub no_color: bool,
    pub is_tmux: bool,
    pub is_asciinema: bool,
    pub is_ci: bool,
    pub truecolor: bool,
    /// OSC 8 hyperlinks recognised by the host terminal.
    pub hyperlinks: bool,
    /// OSC 52 clipboard *write*.  Read is deliberately unprobed: too many
    /// terminals gate it behind a permission prompt.
    pub clipboard_write: bool,
    /// Bracketed paste, taken to follow `supports_ansi` rather than costing a
    /// round-trip query — it is universal in modern ANSI terminals.
    pub bracketed_paste: bool,
    pub mode: InteractiveMode,
}

impl TerminalState {
    /// Probe in `Auto`, for contexts with no env of their own to consult (a
    /// re-exec'd pipeline-stage child).
    pub fn probe() -> Self {
        Self::probe_with_mode(InteractiveMode::Auto)
    }

    /// Probe in the mode `RAL_INTERACTIVE_MODE` names.  The env var is read
    /// here, beside the type defining the modes, rather than respelled in each
    /// frontend.
    #[allow(clippy::disallowed_methods)] // mode selector, not a basedir
    pub fn probe_from_env() -> (InteractiveMode, Self, Option<String>) {
        let raw = std::env::var("RAL_INTERACTIVE_MODE").ok();
        let (mode, warn) = InteractiveMode::parse(raw.as_deref());
        (mode, Self::probe_with_mode(mode), warn)
    }

    /// Query the OS and environment.  Callers seed `crate::ansi::set_terminal`
    /// with the result so process-wide color gating agrees with this snapshot.
    // TMUX / ASCIINEMA_REC are presence probes, not basedirs.
    #[allow(clippy::disallowed_methods)]
    pub fn probe_with_mode(mode: InteractiveMode) -> Self {
        let (startup_stdin_tty, startup_stdout_tty, startup_stderr_tty) = probe_isatty();
        let startup_foreground = probe_foreground(startup_stdin_tty);
        let env = TerminalEnv::from_process();

        let supports_ansi = ansi_supported(mode, startup_stdout_tty);
        let stderr_ansi_capable =
            matches!(mode, InteractiveMode::Full) || anstyle_query::term_supports_ansi_color();
        let modern_osc = env.recognises_modern_osc();

        Self {
            startup_stdin_tty,
            startup_stdout_tty,
            startup_stderr_tty,
            startup_foreground,
            supports_ansi,
            stderr_ansi_capable,
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
    // Each `ui_*_ok` is "raw capability bit ∧ mode/user policy".  Code that
    // must override the policy — forcing OSC 8 while debugging, say — reads
    // the raw field instead.

    /// UI may emit styling.
    pub fn ui_ansi_ok(&self) -> bool {
        !self.mode.is_minimal() && self.supports_ansi && !self.no_color
    }

    /// Terminal round-trip queries (CPR, DA, OSC) are appropriate.
    pub fn ui_round_trips_ok(&self) -> bool {
        self.startup_stdout_tty && !self.mode.is_minimal()
    }

    /// Terminal title may be set via OSC 0/2 sequences.
    pub fn ui_title_ok(&self) -> bool {
        self.ui_round_trips_ok()
    }

    /// 24-bit foreground/background colors may be emitted.
    pub fn ui_truecolor_ok(&self) -> bool {
        self.ui_ansi_ok() && self.truecolor
    }

    /// OSC 8 hyperlinks may be emitted.  `NO_COLOR` does not block them: they
    /// are structure, not color.
    pub fn ui_hyperlinks_ok(&self) -> bool {
        self.ui_round_trips_ok() && self.hyperlinks
    }

    /// OSC 52 clipboard writes may be emitted.
    pub fn ui_clipboard_write_ok(&self) -> bool {
        self.ui_round_trips_ok() && self.clipboard_write
    }

    /// Bracketed-paste mode may be enabled by the line editor.
    pub fn ui_bracketed_paste_ok(&self) -> bool {
        self.ui_round_trips_ok() && self.bracketed_paste
    }

    /// Diagnostics may emit ANSI.  Separate from `ui_ansi_ok` because stderr
    /// can be a tty while stdout is piped to a pager, and errors should still
    /// be colored there.
    pub fn stderr_ansi_ok(&self) -> bool {
        !self.mode.is_minimal()
            && !self.no_color
            && self.startup_stderr_tty
            && self.stderr_ansi_capable
    }

    /// Project the snapshot into the `$TERMINAL` map bound for RC files and
    /// plugins.  Scripts pattern-match these keys: adding one is safe,
    /// renaming or removing one breaks them.
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

// ── isatty and ANSI gating ────────────────────────────────────────────────

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

/// `tcgetpgrp(stdin) == getpgrp()`, and `false` whenever stdin is not a tty.
///
/// Windows has no `tcsetpgrp` and shares a console between attached processes,
/// so a console-attached shell counts as owning the foreground there.
fn probe_foreground(stdin_tty: bool) -> bool {
    if !stdin_tty {
        return false;
    }
    #[cfg(unix)]
    {
        rustix::termios::tcgetpgrp(rustix::stdio::stdin())
            .is_ok_and(|fg| fg == rustix::process::getpgrp())
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// ANSI styling is acceptable on stdout: `Full` forces it on even when piped,
/// `Minimal` off, `Auto` defers to TERM and isatty.
fn ansi_supported(mode: InteractiveMode, stdout_tty: bool) -> bool {
    match mode {
        InteractiveMode::Full => true,
        InteractiveMode::Minimal => false,
        InteractiveMode::Auto => stdout_tty && anstyle_query::term_supports_ansi_color(),
    }
}

// ── Environment snapshot for the capability probes ────────────────────────

/// The env the capability probes consult, captured once so each probe is a
/// pure function of data that a test can build as a literal.
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
    // KITTY_WINDOW_ID / WT_SESSION are presence probes, not basedirs.
    #[allow(clippy::disallowed_methods)]
    fn from_process() -> Self {
        Self {
            term: std::env::var("TERM").ok(),
            term_program: std::env::var("TERM_PROGRAM").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            vte_version: std::env::var("VTE_VERSION").ok(),
            kitty_window_id: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            wt_session: std::env::var_os("WT_SESSION").is_some(),
        }
    }

    /// `COLORTERM` is the de-facto 24-bit signal every major terminal honours.
    fn advertises_truecolor(&self) -> bool {
        matches!(self.colorterm.as_deref(), Some("truecolor" | "24bit"))
    }

    /// The host is in the cohort honouring both OSC 8 and OSC 52; any one
    /// signal suffices.  5000 is VTE 0.50, the first release with hyperlinks.
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

/// Snapshot stdin's console mode for [`restore_console_mode`] — the Windows
/// counterpart of `tcgetattr`, and used the same way by the REPL's panic hook
/// to undo raw mode before writing a crash log. `None` when stdin is not a
/// console.
#[cfg(windows)]
#[allow(
    clippy::too_long_first_doc_paragraph,
    reason = "the summary is one sentence with no interior stop: its only seam is an em dash, so a paragraph break there would leave rustdoc's item list an unterminated clause and open the next paragraph with a dangling dash"
)]
pub fn console_mode_snapshot() -> Option<u32> {
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode: u32 = 0;
    (unsafe { GetConsoleMode(h, &raw mut mode) } != 0).then_some(mode)
}

/// Restore a mode captured by [`console_mode_snapshot`] onto stdin.
#[cfg(windows)]
pub fn restore_console_mode(mode: u32) {
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    unsafe {
        SetConsoleMode(h, mode);
    }
}

/// Windows `isatty`: `GetConsoleMode` succeeds only on a real console handle,
/// never on a pipe, file, or NUL.
#[cfg(windows)]
pub(crate) fn is_console(std_handle: u32) -> bool {
    let h = unsafe { GetStdHandle(std_handle) };
    let mut mode: u32 = 0;
    unsafe { GetConsoleMode(h, &raw mut mode) != 0 }
}

/// Enable ANSI virtual-terminal processing on the stdout and stderr console
/// handles.
///
/// `main` must call this before anything writes an escape: the bundled uutils
/// emit escapes through libc and rely on the host having switched the console
/// into VTP mode first.  A no-op on a handle redirected to a pipe or file.
#[cfg(windows)]
pub fn enable_virtual_terminal_processing() {
    const ENABLE_VTP: u32 = 0x0004;
    for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = unsafe { GetStdHandle(id) };
        let mut mode: u32 = 0;
        if unsafe { GetConsoleMode(h, &raw mut mode) } != 0 {
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

    /// Mirrors the production probe in the one way the predicates care about:
    /// the extra capabilities ride on `supports_ansi`.
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
            stderr_ansi_capable: matches!(mode, InteractiveMode::Full)
                || anstyle_query::term_supports_ansi_color(),
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
        // NO_COLOR beats Full: user intent overrides the force.
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
        // Full on a tty without NO_COLOR wins regardless of the TERM checks.
        assert!(make_state(InteractiveMode::Full, false, false, true).stderr_ansi_ok());
    }

    // ── Capability probes ─────────────────────────────────────────────────

    /// Positional `TerminalEnv` builder; `None` means unset.
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
        // NO_COLOR is set above; hyperlinks are structure, not color.
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
            stderr_ansi_capable: true,
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
