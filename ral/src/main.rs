//! Entry point for the `ral` interactive shell binary.
//!
//! Parses argv to select a mode (interactive REPL, script, `-c` command,
//! or login shell), bootstraps the prelude from a build-time-baked IR blob,
//! and dispatches accordingly.  Batch execution (scripts and `-c`) happens
//! entirely in this module; interactive sessions are handed off to [`repl`].

use clap::{CommandFactory as _, Parser as _};
use ral_core::types::{Break, Escape, Settled};
use ral_core::{
    RequestedTerminalAccess, Shell, TurnIo, TurnReport, TurnRequest, TurnStdin, diagnostic,
    elaborator::elaborate, syntax::parser::parse,
};
use std::process::ExitCode;
use std::sync::Arc;

mod jobs;
mod platform;
mod repl;

pub(crate) use platform::{load_exit_hints, probe_terminal};

// ── Baked prelude ─────────────────────────────────────────────────────────

/// The prelude baked into this binary at build time by `build.rs`.
pub(crate) static PRELUDE: ral_core::driver::BakedPrelude = ral_core::baked_prelude!();

// ── Mode / options ────────────────────────────────────────────────────────

/// Execution mode derived from argv.  Each variant carries exactly the
/// flags valid for it, so misassignment between modes is unrepresentable.
///
/// `Login` is the interactive REPL with login-profile sourcing; it carries
/// the same [`InteractiveOpts`] as `Interactive` so `--norc` and the rest
/// survive (a login shell with `-c` or a script positional resolves to
/// `Command`/`Script` instead — the login bit only distinguishes the
/// interactive case).
enum Mode {
    Login(InteractiveOpts),
    Interactive(InteractiveOpts),
    Script {
        path: String,
        script_args: Vec<String>,
        batch: BatchOpts,
    },
    Command {
        code: String,
        script_args: Vec<String>,
        batch: BatchOpts,
    },
}

/// Universal flags carried with every mode.
#[derive(Default, Clone)]
pub(crate) struct RunOpts {
    /// `--recursion-limit N` — overrides the rc default and the built-in.
    pub recursion_limit: Option<usize>,
    /// `--capabilities a.ral,b.ral[,c.ral]` — capability profiles loaded
    /// at session start.  Multiple files compose left-to-right by `meet`
    /// (each successive file narrows authority); the result is frozen
    /// once and pushed as a permanent frame above `Capabilities::root()`.
    /// Repeated `--capabilities` invocations append.
    pub capabilities: Vec<std::path::PathBuf>,
}

/// Flags valid only in batch (script / `-c`) modes.
#[derive(Default, Clone)]
pub(crate) struct BatchOpts {
    pub audit: bool,
    pub pretty: bool,
    pub check: bool,
    pub dump_ast: bool,
    pub run: RunOpts,
}

/// Flags valid only in the interactive REPL.
#[derive(Default, Clone)]
pub(crate) struct InteractiveOpts {
    pub no_rc: bool,
    /// `-i` — force interactive mode even when stdin is not a tty.
    pub force_interactive: bool,
    /// `-s` — read stdin as a batch script even when a script positional
    /// is present or stdin is a tty.  Takes precedence over `force_interactive`.
    pub force_stdin: bool,
    /// `--surface` — the interactive frontend to present.  `None` leaves the
    /// choice to the rc `surface:` key, falling back to the default surface.
    pub surface: Option<crate::repl::Surface>,
    pub run: RunOpts,
}

/// Parsed argv surface, built by clap.
///
/// `-c` is a bool flag rather than a value-taking flag so that everything
/// after it (the inline code and any trailing positionals) is captured
/// verbatim by `rest` via `trailing_var_arg`.  This mirrors the lexopt
/// behaviour exactly: once `-c` is seen, the remainder is slurped without
/// further flag parsing, even if items look like flags.
#[derive(clap::Parser, Debug)]
#[command(
    name = "ral",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("RAL_GIT_HASH")),
    about = "ral — a typed, structured shell",
    long_about = "\
ral is a shell with a typed, structured value model.  Commands return \
structured values — lists, records, strings, numbers — not raw bytes.  \
The type checker catches errors before execution.

With no arguments ral starts an interactive session.  With a script path \
it runs the script.  With -c it executes inline code.

SCRIPT ARGUMENTS
    Arguments after <script> (or after <code> in -c mode) are available \
inside the program as $args (a list of strings).  The script's own path \
is in $script.  Under -c and in the REPL, $script is unbound.

ENVIRONMENT
    RAL_INTERACTIVE_MODE    line-editing mode: auto (default) or minimal
    RAL_PATH                platform-separated directories (':' on Unix,
                            ';' on Windows) searched for plugins and
                            loaded modules
    RAL_TIMING              if set, print phase timings on stderr
                            (parse / elaborate / typecheck / evaluate)

FILES
    ~/.ralrc                user rc file; sourced at interactive startup
    $XDG_CONFIG_HOME/ral/rc preferred rc location when XDG_CONFIG_HOME is set
    ~/.ral_profile          user login profile (login shells only)
    /etc/ral/profile        system login profile (login shells only)

DEBUGGING EXTERNAL COMMANDS
    Use audit { ... } inside a script to record the exact argv and \
environment handed to execve(2) for each external command.  Render \
the result with to-json.

SEE ALSO
    docs/TUTORIAL.md   — task-oriented introduction
    docs/RATIONALE.md  — design rationale and language overview",
)]
struct Cli {
    /// Start as a login shell; sources login profiles
    #[arg(long, short = 'l')]
    login: bool,

    /// After execution, emit a JSON execution tree to stderr (requires a script or -c)
    #[arg(long)]
    audit: bool,

    /// Pretty-print --audit output
    #[arg(long, requires = "audit")]
    pretty: bool,

    /// Parse and type-check; do not execute
    #[arg(long, short = 'n')]
    check: bool,

    /// Print the parsed AST to stderr; do not execute
    #[arg(long = "dump-ast")]
    dump_ast: bool,

    /// Maximum function-call recursion depth (default 1024; overrides rc recursion_limit:)
    #[arg(long = "recursion-limit", value_name = "N",
          value_parser = clap::value_parser!(u64).range(1..))]
    recursion_limit: Option<u64>,

    /// Comma-separated .ral capability profile paths loaded at session start; may be repeated
    #[arg(long, value_name = "PATHS", value_delimiter = ',',
          action = clap::ArgAction::Append)]
    capabilities: Vec<std::path::PathBuf>,

    /// Treat the next positional as ral code; remaining positionals become $args
    #[arg(short = 'c')]
    code: bool,

    /// Force interactive mode even when stdin is not a terminal
    #[arg(short = 'i')]
    force_interactive: bool,

    /// Read stdin as a script even when a positional argument or terminal is present
    #[arg(short = 's')]
    force_stdin: bool,

    /// Accepted for POSIX $SHELL compatibility; no effect
    #[arg(short = 'e', hide = true)]
    posix_e: bool,

    /// Accepted for POSIX $SHELL compatibility; no effect
    #[arg(short = 'u', hide = true)]
    posix_u: bool,

    /// Skip rc and profile files
    #[arg(long, visible_alias = "noprofile")]
    norc: bool,

    /// Interactive surface: readline (default), minimal, or structural; overrides rc surface:
    #[arg(long, value_enum, value_name = "SURFACE")]
    surface: Option<crate::repl::Surface>,

    /// Script path + trailing args, or (with -c) inline code + trailing args.
    /// Supply after `--` explicitly, or let the binary inject it for you.
    #[arg(last = true, value_name = "ARG")]
    rest: Vec<String>,
}

impl Cli {
    /// Distil the parsed flags into the right [`Mode`] variant.
    ///
    /// The login bit (`-l` or a `-`-prefixed argv\[0\]) does not short-
    /// circuit: a login shell invoked with `-c` or a script positional —
    /// as cron, `su -`, and display-manager `$SHELL -l -c …` all do — must
    /// still run that command rather than dropping it for an interactive
    /// REPL.  Login therefore only selects between the two interactive
    /// variants, decided after `-c`/script are ruled out.
    fn into_mode(self) -> Mode {
        let is_login = self.login || is_login_shell_argv0();

        let caps: Vec<_> = self
            .capabilities
            .into_iter()
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        let run = RunOpts {
            recursion_limit: self.recursion_limit.map(|n| n as usize),
            capabilities: caps,
        };
        let batch = BatchOpts {
            audit: self.audit,
            pretty: self.pretty,
            check: self.check,
            dump_ast: self.dump_ast,
            run: run.clone(),
        };

        if self.code {
            let mut it = self.rest.into_iter();
            let code = it.next().unwrap_or_else(|| {
                Cli::command()
                    .error(
                        clap::error::ErrorKind::MissingRequiredArgument,
                        "-c requires an argument",
                    )
                    .exit()
            });
            return Mode::Command {
                code,
                script_args: it.collect(),
                batch,
            };
        }

        if !self.rest.is_empty() {
            let mut it = self.rest.into_iter();
            let path = it.next().unwrap();
            return Mode::Script {
                path,
                script_args: it.collect(),
                batch,
            };
        }

        // No script, no -c — batch-only flags are meaningless.
        if self.audit || self.check || self.dump_ast {
            let flag = if self.audit {
                "--audit"
            } else if self.check {
                "--check"
            } else {
                "--dump-ast"
            };
            Cli::command()
                .error(
                    clap::error::ErrorKind::MissingRequiredArgument,
                    format!("{flag} requires a script or -c"),
                )
                .exit();
        }

        let opts = InteractiveOpts {
            no_rc: self.norc,
            force_interactive: self.force_interactive,
            force_stdin: self.force_stdin,
            surface: self.surface,
            run,
        };
        if is_login {
            Mode::Login(opts)
        } else {
            Mode::Interactive(opts)
        }
    }
}

// ── Batch execution ───────────────────────────────────────────────────────

/// Serialise the execution tree root to JSON and emit it on stderr.
fn emit_audit_tree(
    name: &str,
    result: &Settled<ral_core::types::Value>,
    exit_code: i32,
    tree_children: Vec<ral_core::types::ExecNode>,
    audit_start: i64,
    pretty: bool,
    principal: String,
) {
    use ral_core::types::{ExecNode, ExecNodeKind, Value};
    let (value, err_msg) = match result {
        Ok(v) => (v.clone(), String::new()),
        Err(Break::Error(e)) => (Value::Unit, e.message.clone()),
        Err(Break::Escape(_)) => (Value::Unit, String::new()),
    };
    let root = ExecNode {
        kind: ExecNodeKind::Command,
        cmd: name.to_string(),
        args: Vec::new(),
        status: exit_code,
        script: name.to_string(),
        line: 0,
        col: 0,
        stdout: Vec::new(),
        stderr: err_msg.into_bytes(),
        value,
        children: tree_children,
        start: audit_start,
        end: ral_core::types::epoch_us(),
        principal,
    };
    let json_val = ral_core::builtins::value_to_json_lossy_bytes(&root.to_value());
    let json_str = if pretty {
        serde_json::to_string_pretty(&json_val).unwrap_or_default()
    } else {
        serde_json::to_string(&json_val).unwrap_or_default()
    };
    eprintln!("{json_str}");
}

/// Apply the `--capabilities` profiles as a session-wide ceiling, mapping
/// the outcome to a process exit.
///
/// The composition mechanism (load, `meet`-fold, freeze, push) lives in
/// [`ral_core::capability::apply_session_profiles`].  A load failure is
/// attributed to the flag that supplied the profiles and yields exit 2; an
/// escape raised while a profile evaluates (`exit`, a stopped child)
/// propagates to the same process exit it would from any other script.
pub(crate) fn apply_session_capabilities(
    shell: &mut Shell,
    paths: &[std::path::PathBuf],
) -> Result<(), ExitCode> {
    match ral_core::capability::apply_session_profiles(shell, paths) {
        Ok(()) => Ok(()),
        Err(Break::Error(e)) => {
            diagnostic::cmd_error("ral", &format!("--capabilities: {}", e.message));
            Err(ExitCode::from(2))
        }
        Err(Break::Escape(Escape::Exit(code))) => Err(ExitCode::from(code.clamp(0, 255) as u8)),
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => Err(ExitCode::from(1)),
    }
}

/// Execute `source` non-interactively (script or `-c` mode).
///
/// Parses, elaborates, optionally typechecks, and evaluates the program.
/// When `--audit` is active, wraps the entire execution in a traced tree
/// and emits it as JSON on stderr.
fn run_batch(name: &str, source: String, script_args: Vec<String>, opts: BatchOpts) -> ExitCode {
    let BatchOpts {
        audit,
        pretty,
        check,
        dump_ast,
        run: RunOpts {
            recursion_limit,
            capabilities,
        },
    } = opts;
    ral_core::process::install_handlers();
    // Seed the ANSI color gate so `_ansi-ok` and the prelude ansi-* constants
    // work correctly in batch (script / -c) mode, not just the REPL.
    let (_, terminal) = probe_terminal(false);

    // RAL_TIMING is a presence probe, not a basedir.
    #[allow(clippy::disallowed_methods)]
    let timing = std::env::var_os("RAL_TIMING").is_some();
    let t0 = std::time::Instant::now();
    macro_rules! tick {
        ($label:literal) => {
            if timing {
                eprintln!(
                    "[timing] {:12} {:.3}ms",
                    $label,
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        };
    }
    // Check `comp`, seeded from the baked prelude list — a batch script has
    // no prior session, so the prelude is the whole seed.  The inference pass
    // always runs: it is what writes the evaluator's mode wires.  A clean
    // check returns the annotated comp; any type error is fatal.
    let run_check =
        |comp: &ral_core::ir::Comp| -> Result<ral_core::ir::Comp, Vec<ral_core::TypeError>> {
            ral_core::typecheck(
                comp,
                ral_core::SessionSchemes::from_schemes(PRELUDE.schemes()),
            )
        };

    let ast = match parse(&source) {
        Ok(ast) => ast,
        Err(e) => {
            eprint!(
                "{}",
                diagnostic::format_parse_error_ariadne(name, &source, &e)
            );
            return ExitCode::from(2);
        }
    };
    tick!("parse");

    if dump_ast {
        for node in &ast {
            eprintln!("{node:#?}");
        }
        return ExitCode::SUCCESS;
    }

    if check {
        // `--check` is the checker: it reports every error and exits nonzero.
        let comp = elaborate(&ast, Default::default());
        if let Err(errors) = run_check(&comp) {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(name, &source, &errors)
            );
            return ExitCode::from(1u8);
        }
        return ExitCode::SUCCESS;
    }

    let mut shell = ral_core::driver::boot_shell(terminal, &PRELUDE);
    // `watch` is core-implemented but host-installed: a batch script's
    // stdout is the real terminal or pipe, a durable sink a detached
    // watcher can outlive the turn writing to.  Registered process-wide for
    // typecheck by `register_host_surface()`; installed here so it also runs.
    shell.install_builtins(ral_core::builtins::WATCH_BUILTIN);
    shell.set_exit_hints(load_exit_hints());
    tick!("builtins");
    if let Some(n) = recursion_limit {
        shell.mobile.control.recursion_limit = n;
    }
    shell.install_root_context(name.to_string(), source.as_str());
    shell.mobile.context.args = script_args;

    // `--capabilities a.ral,b.ral` — load + freeze + push as session
    // ceiling.  Done after register because profiles may use builtins
    // (`let`, conditionals, the path sigils).  Done before script eval
    // so the session script runs under the resulting frame.
    if let Err(code) = apply_session_capabilities(&mut shell, &capabilities) {
        return code;
    }
    tick!("caps");

    let audit_start = if audit {
        ral_core::types::epoch_us()
    } else {
        0
    };

    if audit {
        shell.enable_audit();
    }

    let bare = elaborate(&ast, Default::default());
    tick!("elaborate");

    // The inference pass always runs — it writes the mode wires the
    // evaluator reads.  A clean check evaluates the fully annotated comp
    // (each top-level bind installs its scheme); any type error is fatal.
    let comp = match run_check(&bare) {
        Ok(annotated) => Arc::new(annotated),
        Err(errs) => {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(name, &source, &errs)
            );
            return ExitCode::from(1u8);
        }
    };
    tick!("typecheck");

    let terminal_access = if shell.terminal().startup_foreground {
        RequestedTerminalAccess::Leased
    } else {
        RequestedTerminalAccess::Denied
    };
    let result = match shell.run_source_turn(
        source.as_str(),
        TurnRequest {
            script_name: name,
            caps: ral_core::types::Capabilities::root(),
            turn_limit: None,
            detached_limit: None,
            io: TurnIo::Inherit,
            terminal: terminal_access,
            stdin: TurnStdin::Inherit,
            surface: None,
            lifecycle: Box::new(()),
        },
    ) {
        TurnReport::Ran { result, .. } => result,
        // Batch already typechecked above, so a static report should not
        // occur here; treat it defensively as a fatal run (exit 1).
        TurnReport::Static { .. } => Err(Break::Escape(Escape::Exit(1))),
    };
    tick!("evaluate");

    let tree_children = if audit {
        shell.take_audit_fragment().into_nodes()
    } else {
        Vec::new()
    };

    let exit_code = match &result {
        Ok(_) => shell.mobile.control.last_status.clamp(0, 255),
        Err(Break::Escape(Escape::Exit(code))) => (*code).clamp(0, 255),
        Err(Break::Error(e)) => {
            if !audit {
                eprint!(
                    "{}",
                    diagnostic::format_runtime_error_auto(
                        shell.sources(),
                        e,
                        ral_core::ir::is_single_command(&comp),
                    )
                );
            }
            e.exit_code().clamp(0, 255)
        }
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => 1,
    };

    // --audit: emit the execution tree as JSON on stderr.
    if audit {
        emit_audit_tree(
            name,
            &result,
            exit_code,
            tree_children,
            audit_start,
            pretty,
            shell.mobile.context.principal(),
        );
    }

    ExitCode::from(exit_code as u8)
}

// ── main ─────────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    #[cfg(windows)]
    ral_core::io::enable_virtual_terminal_processing();

    // Restore SIGPIPE to SIG_DFL once at startup so in-process uutils
    // and pipeline helpers see the default disposition.
    #[cfg(unix)]
    ral_core::builtins::uutils::init_signal_dispositions();
    // Pipeline helper multicall dispatch — the parent spawns
    // `current_exe()` with `--ral-pipeline-stage-helper` to run a
    // pipeline stage in a fresh subprocess.
    if let Some(code) = ral_core::try_run_pipeline_stage_helper() {
        return ExitCode::from(code);
    }
    if let Some(code) = ral_core::test_helper::try_run_test_helper() {
        return ExitCode::from(code);
    }

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (stripped, exit) = match ral_core::sandbox::early_init(&argv) {
        Ok(result) => result,
        Err(e) => {
            diagnostic::cmd_error("ral", &e.clone());
            return ExitCode::from(1);
        }
    };
    if let Some(code) = exit {
        return ExitCode::from(code);
    }

    // Per-command sandbox re-exec tails — run *after* `early_init` so a
    // `--sandbox-projection` child enters the OS sandbox first, then runs
    // the target confined.  `stripped` is the post-early_init argv (sans
    // binary name).  On macOS a leading `--ral-sandbox-exec <program> …`
    // `execve`s the host program inside the Seatbelt just entered; a
    // leading `--ral-bundled-tool <tool> …` runs the bundled tool
    // in-process.  Both exit here, never reaching clap.
    if let Some(code) = ral_core::sandbox::serve_sandbox_exec(&stripped) {
        return ExitCode::from(code);
    }
    if let Some(code) = ral_core::try_run_bundled_tool(&stripped) {
        return ExitCode::from(code);
    }

    let cli =
        Cli::parse_from(std::iter::once("ral".to_string()).chain(inject_arg_terminator(stripped)));
    let mode = cli.into_mode();

    // Refuse to run setuid — the shell inherits the caller's environment and
    // must not run with elevated privileges the user did not request.
    #[cfg(unix)]
    unsafe {
        if libc::geteuid() != libc::getuid() {
            eprintln!("ral: refusing to run setuid");
            return ExitCode::from(1);
        }
    }

    ral_core::builtins::misc::register_prelude_type_hints(PRELUDE.schemes());

    // Publish the ral host surface (`_ed-*` editor builtins and `watch`,
    // including schemes) into core's host table before any builtin lookup
    // runs.  All execution modes — REPL, `-c`, scripts — go through builtin
    // dispatch and type seeding, so registration is unconditional.
    repl::register_host_surface();

    match mode {
        Mode::Login(_) | Mode::Interactive(_) => {
            let is_login = matches!(mode, Mode::Login(_));
            let interactive = match mode {
                Mode::Login(o) | Mode::Interactive(o) => o,
                _ => unreachable!("outer match guarantees Login or Interactive"),
            };

            // Decide between REPL and stdin-as-script.
            //
            // Precedence (highest first):
            //   1. `-s` — force stdin batch regardless of tty or `-i`
            //   2. `-i` — force REPL regardless of tty
            //   3. default — REPL iff stdin is a tty
            let read_stdin_as_script = interactive.force_stdin
                || (!interactive.force_interactive && {
                    #[cfg(unix)]
                    {
                        !std::io::IsTerminal::is_terminal(&std::io::stdin())
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                });

            if read_stdin_as_script {
                use std::io::Read;
                let mut source = String::new();
                if let Err(e) = std::io::stdin().read_to_string(&mut source) {
                    diagnostic::cmd_error("ral", &format!("<stdin>: {e}"));
                    return ExitCode::from(1);
                }
                let source = ral_core::source::normalize_source_text(source);
                return run_batch(
                    "<stdin>",
                    source,
                    vec![],
                    BatchOpts {
                        run: interactive.run,
                        ..BatchOpts::default()
                    },
                );
            }

            repl::run_interactive(is_login, &interactive)
        }
        Mode::Script {
            path,
            script_args,
            batch,
        } => {
            #[allow(
                clippy::disallowed_methods,
                reason = "[io-door:silent:script-read] startup read of the script file path; not turn-time model I/O"
            )]
            let source = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    diagnostic::cmd_error("ral", &format!("{path}: {e}"));
                    return ExitCode::from(1);
                }
            };
            let source = ral_core::source::normalize_source_text(source);
            run_batch(&path, source, script_args, batch)
        }
        Mode::Command {
            code,
            script_args,
            batch,
        } => run_batch("-c", code, script_args, batch),
    }
}

/// Insert `--` so that clap's `last = true` semantics on `rest` capture
/// the script/`-c` remainder uniformly, even when it is flag-shaped.
///
/// Two cases inject the terminator:
/// - immediately after a `-c` (the code that follows is slurped verbatim,
///   so `ral -c '--version'` runs the code instead of clap reading
///   `--version` as the version flag);
/// - before the first non-option positional (the script path), so its
///   trailing arguments are not re-parsed as flags.
///
/// All option flags before that point are parsed by clap normally (with
/// typo suggestions etc.).  A long flag that takes a *separate* value token
/// (`--surface readline`, `--recursion-limit 4096`) carries that value past
/// the flag; we skip it so it is not mistaken for the positional that
/// triggers injection.  Which flags those are is read from clap's own model
/// ([`value_taking_longs`]) rather than hand-listed, so it can never drift
/// from the `Cli` definition — that drift is exactly what made `--surface
/// readline` inject the terminator between the flag and its value.
fn inject_arg_terminator(raw: Vec<String>) -> Vec<String> {
    let value_longs = value_taking_longs();
    let mut out = Vec::with_capacity(raw.len() + 1);
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--" {
            out.extend(raw[i..].iter().cloned());
            return out;
        }
        if arg.starts_with('-') {
            let is_value_flag = value_longs.iter().any(|f| f == arg);
            out.push(arg.clone());
            i += 1;
            // `-c` switches to inline-code mode: the remainder is the code
            // and its own arguments, taken verbatim.  Terminate options
            // here so a flag-shaped code token (`--version`) is not parsed.
            if is_code_flag(arg) {
                out.push("--".to_string());
                out.extend(raw[i..].iter().cloned());
                return out;
            }
            // Consume the separate value token if the flag has one and it
            // isn't embedded via `=` (which would already be in `arg`).
            if is_value_flag && !arg.contains('=') && i < raw.len() {
                out.push(raw[i].clone());
                i += 1;
            }
        } else {
            out.push("--".to_string());
            out.extend(raw[i..].iter().cloned());
            return out;
        }
    }
    out
}

/// The `--long` flags that take a separate value token, read from clap's own
/// argument model so [`inject_arg_terminator`] can never disagree with the
/// `Cli` definition about which flags consume the next token.  ral's
/// value-taking flags are all long-only (`-c` is the special inline-code
/// case, handled separately), so a short-only flag does not arise here.
fn value_taking_longs() -> Vec<String> {
    Cli::command()
        .get_arguments()
        .filter(|arg| arg.get_action().takes_values())
        .filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
        .collect()
}

/// Whether `arg` is the `-c` inline-code flag, alone or as the trailing
/// letter of a single-dash short cluster (`-lc`).  In a cluster the `c`
/// must be last, since the following token is the code it consumes.
fn is_code_flag(arg: &str) -> bool {
    arg.strip_prefix('-')
        .filter(|rest| !rest.starts_with('-') && rest.chars().all(|c| c.is_ascii_alphabetic()))
        .is_some_and(|rest| rest.ends_with('c'))
}

/// True when argv[0] starts with `-`, the POSIX convention indicating
/// that the shell was invoked as a login shell.
fn is_login_shell_argv0() -> bool {
    std::env::args()
        .next()
        .is_some_and(|argv0| ral_core::path::basename(&argv0).starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse argv (without the leading program name) the same way `main`
    /// does — terminator injection, then clap — and distil to a [`Mode`].
    /// argv\[0\] in the test binary never starts with `-`, so the login
    /// bit comes solely from an explicit `-l`.
    fn mode_of(args: &[&str]) -> Mode {
        let raw: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Cli::parse_from(std::iter::once("ral".to_string()).chain(inject_arg_terminator(raw)))
            .into_mode()
    }

    /// J1: a login shell handed `-c`, a script, or flags must honour them,
    /// not silently substitute an interactive REPL with default options.
    #[test]
    fn login_mode_parse_matrix() {
        // Bare login → interactive login, default options.
        match mode_of(&["-l"]) {
            Mode::Login(o) => assert!(!o.no_rc),
            m => panic!("`-l` should be Login, got {}", mode_name(&m)),
        }

        // Login + `-c` runs the command (login profiles are a REPL concern).
        match mode_of(&["-lc", "echo hi"]) {
            Mode::Command { code, .. } => assert_eq!(code, "echo hi"),
            m => panic!("`-lc 'echo hi'` should be Command, got {}", mode_name(&m)),
        }

        // Login flags are carried through, not dropped.
        match mode_of(&["-l", "--norc"]) {
            Mode::Login(o) => assert!(o.no_rc, "`-l --norc` must keep no_rc"),
            m => panic!("`-l --norc` should be Login, got {}", mode_name(&m)),
        }

        // Login + script runs the script.
        match mode_of(&["-l", "script.ral"]) {
            Mode::Script { path, .. } => assert_eq!(path, "script.ral"),
            m => panic!("`-l script.ral` should be Script, got {}", mode_name(&m)),
        }
    }

    /// The non-login modes are unchanged by the reordering.
    #[test]
    fn non_login_mode_parse_matrix() {
        assert!(matches!(mode_of(&[]), Mode::Interactive(_)));
        match mode_of(&["--norc"]) {
            Mode::Interactive(o) => assert!(o.no_rc),
            m => panic!("`--norc` should be Interactive, got {}", mode_name(&m)),
        }
        match mode_of(&["-c", "echo hi"]) {
            Mode::Command { code, .. } => assert_eq!(code, "echo hi"),
            m => panic!("`-c` should be Command, got {}", mode_name(&m)),
        }
        match mode_of(&["script.ral", "arg1"]) {
            Mode::Script {
                path, script_args, ..
            } => {
                assert_eq!(path, "script.ral");
                assert_eq!(script_args, vec!["arg1".to_string()]);
            }
            m => panic!("`script.ral arg1` should be Script, got {}", mode_name(&m)),
        }
    }

    fn mode_name(m: &Mode) -> &'static str {
        match m {
            Mode::Login(_) => "Login",
            Mode::Interactive(_) => "Interactive",
            Mode::Script { .. } => "Script",
            Mode::Command { .. } => "Command",
        }
    }

    fn inject(args: &[&str]) -> Vec<String> {
        inject_arg_terminator(args.iter().map(|s| s.to_string()).collect())
    }

    /// J9: a `-c` whose code is flag-shaped must reach `rest`, not clap's
    /// own flags — `ral -c '--version'` runs the code, it does not print
    /// the banner.
    #[test]
    fn arg_terminator_slurps_code_after_dash_c() {
        // `-c` followed by a flag-shaped token: `--` is injected right
        // after `-c`, so the token is code, not a clap flag.
        assert_eq!(inject(&["-c", "--version"]), vec!["-c", "--", "--version"]);
        // Bundled `-lc`: the trailing `c` is the code flag.
        assert_eq!(inject(&["-lc", "echo hi"]), vec!["-lc", "--", "echo hi"]);
        // Code plus its own trailing args are all slurped verbatim.
        assert_eq!(
            inject(&["-c", "echo hi", "-n"]),
            vec!["-c", "--", "echo hi", "-n"]
        );
    }

    /// The pre-existing behaviour is unchanged: a script positional gets a
    /// `--` before it, value-taking long flags keep their value token, and
    /// a non-code short cluster is not treated as `-c`.
    #[test]
    fn arg_terminator_script_and_value_flags() {
        assert_eq!(
            inject(&["script.ral", "-x"]),
            vec!["--", "script.ral", "-x"]
        );
        assert_eq!(
            inject(&["--recursion-limit", "2048", "script.ral"]),
            vec!["--recursion-limit", "2048", "--", "script.ral"]
        );
        // `-l` alone is not a code flag; the next token still triggers the
        // positional terminator rather than being slurped as code.
        assert_eq!(
            inject(&["-l", "script.ral"]),
            vec!["-l", "--", "script.ral"]
        );
    }

    /// Regression: a value-taking flag added to `Cli` works in its space-
    /// separated form without anyone updating a hand-list.  `--surface` (the
    /// flag whose omission from the old hardcoded list forced the `=` form)
    /// must keep its value rather than have a `--` fenced between them, and so
    /// parse to an interactive surface choice.
    #[test]
    fn value_flag_space_form_derived_from_clap() {
        assert!(
            value_taking_longs().iter().any(|f| f == "--surface"),
            "clap reports --surface as value-taking; the injector must see it"
        );
        assert_eq!(
            inject(&["--surface", "readline"]),
            vec!["--surface", "readline"],
            "the value must stay with its flag, no terminator between them"
        );
        match mode_of(&["--surface", "readline"]) {
            Mode::Interactive(o) => {
                assert!(matches!(o.surface, Some(crate::repl::Surface::Readline)));
            }
            m => panic!(
                "`--surface readline` should be Interactive, got {}",
                mode_name(&m)
            ),
        }
    }
}
