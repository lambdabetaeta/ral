//! In-process ral evaluation against a persistent `Shell`.
//!
//! Loads the baked prelude once, seeds standard env vars, and runs each
//! tool call wrapped in a `grant` block with stdout/stderr captured.
//!
//! Each tool call's stdout and stderr are tee'd: one branch into an
//! in-memory buffer (replayed to the model in conversation history),
//! one branch into a [`StreamingPrinter`] that line-buffers and writes
//! coloured `╎ <line>` lines to the user's terminal as they arrive.

use ral_core::io::{ExternalWrite, Sink, Source};
use ral_core::ir::Comp;
use ral_core::typecheck::Scheme;
use ral_core::types::EvalSignal;
use ral_core::{Shell, Value as RalValue, diagnostic, elaborate, parse, sandbox};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

/// The prelude IR, deserialized on first use.
pub fn baked_prelude_comp() -> &'static Comp {
    static C: OnceLock<Comp> = OnceLock::new();
    C.get_or_init(|| {
        postcard::from_bytes(include_bytes!(concat!(env!("OUT_DIR"), "/prelude_baked.bin")))
            .expect("prelude IR deserialization failed")
    })
}

/// The prelude type schemes, deserialized on first use.
pub fn baked_prelude_schemes() -> &'static [(String, Scheme)] {
    static S: OnceLock<Vec<(String, Scheme)>> = OnceLock::new();
    S.get_or_init(|| {
        postcard::from_bytes(include_bytes!(concat!(env!("OUT_DIR"), "/prelude_schemes.bin")))
            .expect("prelude schemes deserialization failed")
    })
}

/// Seed well-known environment variables into `shell` from the host env.
pub fn seed_default_env(shell: &mut Shell) {
    let v = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "?".into());
    #[allow(clippy::disallowed_methods)]
    let pwd = std::env::current_dir().map_or_else(|_| "/".into(), |p| p.to_string_lossy().into_owned());
    let oldpwd = v("OLDPWD", &pwd);
    let path = if cfg!(windows) {
        "C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\Wbem"
    } else {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    };
    // PWD/OLDPWD live on the process, not in `dynamic.env_vars` or the
    // ral scope — see `Shell::apply_chdir`.
    unsafe {
        std::env::set_var("PWD", &pwd);
        std::env::set_var("OLDPWD", &oldpwd);
    }
    let mut install = |k: &str, val: String| {
        shell.dynamic.set_env_var_or_keep(k, val.clone());
        shell.set(k.into(), RalValue::String(val));
    };
    for (k, val) in [
        ("HOME", std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into())),
        ("USER", user.clone()),
        ("PATH", v("PATH", path)),
        ("SHELL", v("SHELL", "ral")),
        ("TERM", v("TERM", "xterm-256color")),
        ("LANG", v("LANG", "C.UTF-8")),
        ("LOGNAME", v("LOGNAME", &user)),
    ] {
        install(k, val);
    }
    for k in ["TMUX", "TMUX_PANE", "STY", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"] {
        if let Ok(val) = std::env::var(k) { install(k, val); }
    }
    let shlvl = std::env::var("SHLVL").ok().and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0).saturating_add(1).to_string();
    shell.dynamic.set_env_var("SHLVL", shlvl.clone());
    shell.set("SHLVL".into(), RalValue::String(shlvl));
}

/// A successful tool run, broken into named pieces so the caller can
/// render twice (full / capped) without parsing the rendered form.
/// `audit` is pre-rendered JSON when audit was requested.
pub struct ToolResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: Option<String>,
    pub exit: i32,
    pub audit: Option<String>,
}

/// What `run_shell` produces.  `Static` is for parse / type errors —
/// already-formatted ariadne text with no further structure to cap.
pub enum Outcome {
    Ran(ToolResult),
    Static(String),
}

/// Evaluate `cmd` against `shell`, wrapped in `caps`, capturing
/// stdout and stderr into buffers.  Returns the result as named pieces
/// so the caller can render it twice — once full for the terminal,
/// once with per-section caps for the conversation history — without
/// having to parse the rendered form back apart.
pub fn run_shell(
    shell: &mut Shell,
    caps: &ral_core::types::Capabilities,
    cmd: &str,
    audit: bool,
) -> Outcome {
    let name = "<tool>";

    // Clear any stale interrupt from a prior tool call (e.g. a child
    // process killed by SIGINT left SIGNAL_COUNT=1).  Without this the
    // next tool call's first signal::check boundary would abort with
    // "interrupted" — the REPL clears for the same reason.
    ral_core::signal::clear();

    // Constitutional non-escape: the model's source is parsed, elaborated,
    // and wrapped in a Thunk *Value*.  The grant policy is pushed onto the
    // capability stack as a typed `Capabilities`.  There is no source-level
    // `grant { … }` to escape — `sandbox::eval_grant` consumes the Thunk
    // directly under the active capabilities.
    let ast = match parse(cmd) {
        Ok(a) => a,
        Err(e) => {
            return Outcome::Static(diagnostic::format_parse_error_ariadne(
                name, cmd, e.line, e.col, &e.message,
            ));
        }
    };
    let comp = elaborate(&ast, Default::default());
    let type_errors = ral_core::typecheck(&comp, baked_prelude_schemes());
    if !type_errors.is_empty() {
        return Outcome::Static(
            type_errors
                .iter()
                .map(|e| diagnostic::format_type_error_ariadne(name, cmd, e))
                .collect(),
        );
    }

    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout_stream = Arc::new(StreamingPrinter::stdout());
    let stderr_stream = Arc::new(StreamingPrinter::stderr());
    let prev_stdout = std::mem::replace(
        &mut shell.io.stdout,
        Sink::Tee(
            Box::new(Sink::Buffer(stdout_buf.clone())),
            Box::new(Sink::External(stdout_stream.clone() as Arc<dyn ExternalWrite>)),
        ),
    );
    let prev_stderr = std::mem::replace(
        &mut shell.io.stderr,
        Sink::Tee(
            Box::new(Sink::Buffer(stderr_buf.clone())),
            Box::new(Sink::External(stderr_stream.clone() as Arc<dyn ExternalWrite>)),
        ),
    );
    let prev_stdin = std::mem::replace(&mut shell.io.stdin, Source::Terminal);
    let prev_loc_script = std::mem::replace(&mut shell.location.script, name.to_string());
    let prev_loc_call = std::mem::replace(&mut shell.location.call_site.script, name.to_string());
    let prev_loc_source = shell.location.source.replace(Arc::from(cmd));

    let thunk = RalValue::Thunk {
        body: Arc::new(comp.clone()),
        captured: shell.snapshot(),
    };
    let caps = ral_core::types::Capabilities { audit, ..caps.clone() };
    let run = |sh: &mut Shell| sh.with_capabilities(caps.clone(), |s| sandbox::eval_grant(&thunk, s));
    let (audit_tree, result) = if audit {
        shell.with_audit_scope(run)
    } else {
        (Vec::new(), run(shell))
    };

    shell.io.stdout = prev_stdout;
    shell.io.stderr = prev_stderr;
    shell.io.stdin = prev_stdin;
    shell.location.script = prev_loc_script;
    shell.location.call_site.script = prev_loc_call;
    shell.location.source = prev_loc_source;

    // A child that wrote a partial last line (no terminating '\n') leaves
    // bytes parked in the streaming printer; flush them so they reach the
    // user before we render EXIT.
    stdout_stream.flush();
    stderr_stream.flush();

    let stdout_bytes = std::mem::take(&mut *stdout_buf.lock().unwrap());
    let mut stderr_bytes = std::mem::take(&mut *stderr_buf.lock().unwrap());

    let (exit, value) = match &result {
        Ok(v) => (0, Some(v.clone())),
        Err(EvalSignal::Exit(code)) => ((*code).clamp(0, 255), None),
        Err(EvalSignal::Error(e)) => {
            let rendered = diagnostic::format_runtime_error_auto(name, cmd, e, comp.is_single_command());
            stderr_bytes.extend_from_slice(rendered.as_bytes());
            (e.status.clamp(0, 255), None)
        }
        Err(_) => (1, None),
    };

    let value_str = match value {
        Some(v) if !matches!(v, RalValue::Unit) => {
            let json = ral_core::builtins::value_to_json_pub(&v);
            serde_json::to_string_pretty(&json).ok()
        }
        _ => None,
    };
    let audit = if audit && !audit_tree.is_empty() {
        let list = RalValue::List(audit_tree.iter().map(|n| n.to_value()).collect());
        let json = ral_core::builtins::value_to_json_pub(&list);
        serde_json::to_string_pretty(&json).ok()
    } else {
        None
    };

    Outcome::Ran(ToolResult {
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        value: value_str,
        exit,
        audit,
    })
}

/// Line-buffering ANSI-tinted writer used as the streaming branch of
/// the stdout/stderr `Sink::Tee`.  Bytes accumulate until a newline,
/// then `prefix + line + suffix + '\n'` is written to stderr in one
/// shot — keeping each line atomic and matching the colour scheme
/// `ui::tool_result` uses for the captured-and-replayed case.
///
/// Two sinks share the underlying terminal (one per stream), so
/// stderr's lock around the per-line `write_all` is what prevents
/// stdout's "checking…" lines from interleaving with stderr's "warning:"
/// lines mid-byte.
pub struct StreamingPrinter {
    prefix: &'static str,
    suffix: &'static str,
    pending: Mutex<Vec<u8>>,
}

const STREAM_RESET: &str = "\x1b[0m";
// Lime ╎ rail, dim slate body — same palette as ui::tool_result's
// STDOUT branch.
const STREAM_STDOUT: &str =
    "\x1b[38;2;57;255;20m╎\x1b[0m \x1b[2m\x1b[38;2;140;150;170m";
// Lime rail, dim orange body — matches STDERR.
const STREAM_STDERR: &str =
    "\x1b[38;2;57;255;20m╎\x1b[0m \x1b[2m\x1b[38;2;255;95;31m";

impl StreamingPrinter {
    pub fn stdout() -> Self {
        Self {
            prefix: STREAM_STDOUT,
            suffix: STREAM_RESET,
            pending: Mutex::new(Vec::new()),
        }
    }

    pub fn stderr() -> Self {
        Self {
            prefix: STREAM_STDERR,
            suffix: STREAM_RESET,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Emit any partial trailing line still parked in the buffer.
    /// Called once at the end of a tool run.
    pub fn flush(&self) {
        let mut pending = self.pending.lock().unwrap();
        if pending.is_empty() {
            return;
        }
        let line = std::mem::take(&mut *pending);
        drop(pending);
        let _ = self.emit_line(&line);
    }

    fn emit_line(&self, line: &[u8]) -> io::Result<()> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let mut out = io::stderr().lock();
        out.write_all(self.prefix.as_bytes())?;
        out.write_all(line)?;
        out.write_all(self.suffix.as_bytes())?;
        out.write_all(b"\n")
    }
}

impl ExternalWrite for StreamingPrinter {
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut pending = self.pending.lock().unwrap();
        pending.extend_from_slice(bytes);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            // `line` includes the trailing '\n'; strip before emit so we
            // own the line ending and can place the suffix before it.
            let body = &line[..line.len() - 1];
            // Drop the lock around stderr I/O so a second writer (the
            // sibling stream) can buffer concurrently while we emit.
            let body_owned = body.to_vec();
            drop(pending);
            self.emit_line(&body_owned)?;
            pending = self.pending.lock().unwrap();
        }
        Ok(())
    }
}
