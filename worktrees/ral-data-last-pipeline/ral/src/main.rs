//! Entry point for the `ral` interactive shell binary.
//!
//! Parses argv to select a mode (interactive REPL, script, `-c` command,
//! or login shell), bootstraps the prelude from a build-time-baked IR blob,
//! and dispatches accordingly.  Batch execution (scripts and `-c`) happens
//! entirely in this module; interactive sessions are handed off to [`repl`].

use ral_core::ir::Comp;
use ral_core::typecheck::Scheme;
use ral_core::{Shell, EvalSignal, elaborate, evaluate, parse};
use ral_core::{builtins, diagnostic};
use std::process::ExitCode;
use std::sync::OnceLock;

mod platform;
mod repl;

#[cfg(unix)]
mod jobs;

pub(crate) use platform::{home_dir, probe_terminal, seed_default_env, user_name};

const USAGE: &str = include_str!("../../data/usage.txt");
const HELP: &str = include_str!("../../data/help.txt");

// ── Baked prelude ─────────────────────────────────────────────────────────

/// Lazily deserialise the prelude IR from the build-time artifact.
/// The binary is embedded via `include_bytes!` and decoded with `postcard`.
fn baked_prelude_comp() -> &'static Comp {
    static C: OnceLock<Comp> = OnceLock::new();
    C.get_or_init(|| {
        postcard::from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/prelude_baked.bin"
        )))
        .expect("prelude IR deserialization failed")
    })
}

/// Lazily deserialise the prelude type schemes from the build-time artifact.
pub(crate) fn baked_prelude_schemes() -> &'static [(String, Scheme)] {
    static S: OnceLock<Vec<(String, Scheme)>> = OnceLock::new();
    S.get_or_init(|| {
        postcard::from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/prelude_schemes.bin"
        )))
        .expect("prelude schemes deserialization failed")
    })
}

// ── Mode / options ────────────────────────────────────────────────────────

/// Execution mode derived from argv.  Each variant carries exactly the
/// flags valid for it, so misassignment between modes is unrepresentable.
enum Mode {
    Login,
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
#[derive(Default, Clone, Copy)]
pub(crate) struct RunOpts {
    /// `--recursion-limit N` — overrides the rc default and the built-in.
    pub recursion_limit: Option<usize>,
    /// `--no-typecheck` — skip the static checker (rc files, scripts, REPL lines).
    pub no_typecheck: bool,
}

/// Flags valid only in batch (script / `-c`) modes.
#[derive(Default, Clone, Copy)]
pub(crate) struct BatchOpts {
    pub audit: bool,
    pub pretty: bool,
    pub check: bool,
    pub dump_ast: bool,
    pub run: RunOpts,
}

/// Flags valid only in the interactive REPL.
#[derive(Default, Clone, Copy)]
pub(crate) struct InteractiveOpts {
    pub no_rc: bool,
    pub run: RunOpts,
}

/// Bag for flags as they accumulate during argv parsing.  At end-of-args
/// it's distilled into the right `Mode` variant; misassignment between
/// modes is therefore unrepresentable downstream.
#[derive(Default)]
struct ArgBag {
    batch: BatchOpts,
    interactive: InteractiveOpts,
}

/// Parse argv into a mode.
///
/// Recognises the standard flags (`-c`, `--check`, `--audit`, etc.) and
/// silently absorbs POSIX flags (`-i`, `-s`, `-e`, `-u`) that tools like
/// tmux pass blindly to `$SHELL`.
fn parse_args(raw: &[String]) -> Result<Mode, String> {
    if is_login_shell_argv0() {
        return Ok(Mode::Login);
    }
    let mut bag = ArgBag::default();
    let mut i = 0;

    while i < raw.len() {
        match raw[i].as_str() {
            "--login" | "-l" => return Ok(Mode::Login),
            "--audit" => bag.batch.audit = true,
            "--pretty" => bag.batch.pretty = true,
            "--check" | "-n" => bag.batch.check = true,
            "--dump-ast" => bag.batch.dump_ast = true,
            "--no-typecheck" => {
                bag.batch.run.no_typecheck = true;
                bag.interactive.run.no_typecheck = true;
            }
            "--recursion-limit" => {
                i += 1;
                if i >= raw.len() {
                    return Err("--recursion-limit requires a positive integer".into());
                }
                let n: usize = raw[i]
                    .parse()
                    .map_err(|_| format!("--recursion-limit: not a number: {}", raw[i]))?;
                if n == 0 {
                    return Err("--recursion-limit must be > 0".into());
                }
                bag.batch.run.recursion_limit = Some(n);
                bag.interactive.run.recursion_limit = Some(n);
            }
            "-c" => {
                i += 1;
                if i >= raw.len() {
                    return Err("-c requires an argument".into());
                }
                let code = raw[i].clone();
                let script_args = raw[i + 1..].to_vec();
                return Ok(Mode::Command {
                    code,
                    script_args,
                    batch: bag.batch,
                });
            }
            "--" => {
                i += 1;
                if i < raw.len() {
                    let path = raw[i].clone();
                    let script_args = raw[i + 1..].to_vec();
                    return Ok(Mode::Script {
                        path,
                        script_args,
                        batch: bag.batch,
                    });
                }
                break; // ral -- with nothing after → interactive
            }
            // POSIX flags passed by tmux, Claude Code, and other tools that
            // invoke $SHELL without knowing what shell it is.  Silently ignore
            // them so ral can be used as a default shell.
            "-i" | "-s" | "-e" | "-u" => {}
            "--norc" | "--noprofile" => bag.interactive.no_rc = true,
            arg if arg.starts_with('-') => {
                return Err(format!("unknown flag: {arg}"));
            }
            _ => {
                let path = raw[i].clone();
                let script_args = raw[i + 1..].to_vec();
                return Ok(Mode::Script {
                    path,
                    script_args,
                    batch: bag.batch,
                });
            }
        }
        i += 1;
    }

    // Reached EOA without -c, --, or a positional.  Anything in batch.* is
    // misplaced — those flags require a body to run.
    let b = &bag.batch;
    if b.pretty && !b.audit {
        return Err("--pretty requires --audit".into());
    }
    if b.audit {
        return Err("--audit requires a script or -c".into());
    }
    if b.check {
        return Err("--check requires a script or -c".into());
    }
    if b.dump_ast {
        return Err("--dump-ast requires a script or -c".into());
    }

    Ok(Mode::Interactive(bag.interactive))
}

// ── Batch execution ───────────────────────────────────────────────────────

/// Serialise the execution tree root to JSON and emit it on stderr.
fn emit_audit_tree(
    name: &str,
    result: &Result<ral_core::types::Value, ral_core::types::EvalSignal>,
    exit_code: i32,
    tree_children: Vec<ral_core::types::ExecNode>,
    audit_start: i64,
    pretty: bool,
) {
    use ral_core::types::{ExecNode, ExecNodeKind, Value};
    let (value, err_msg) = match result {
        Ok(v) => (v.clone(), String::new()),
        Err(ral_core::types::EvalSignal::Error(e)) => (Value::Unit, e.message.clone()),
        Err(_) => (Value::Unit, String::new()),
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
        principal: user_name(),
    };
    let json_val = ral_core::builtins::value_to_json_audit(&root.to_value());
    let json_str = if pretty {
        serde_json::to_string_pretty(&json_val).unwrap_or_default()
    } else {
        serde_json::to_string(&json_val).unwrap_or_default()
    };
    eprintln!("{json_str}");
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
            no_typecheck,
        },
    } = opts;
    ral_core::signal::install_handlers();
    // Seed the ANSI color gate so `_ansi-ok` and the prelude ansi-* constants
    // work correctly in batch (script / -c) mode, not just the REPL.
    let (_, terminal) = probe_terminal(false);

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
    // Render every type error to stderr; return true iff any were found.
    let render_type_errors = |comp: &ral_core::ir::Comp| -> bool {
        let type_errors = ral_core::typecheck(comp, baked_prelude_schemes());
        for e in &type_errors {
            eprint!("{}", diagnostic::format_type_error_ariadne(name, &source, e));
        }
        !type_errors.is_empty()
    };

    let ast = match parse(&source) {
        Ok(ast) => ast,
        Err(e) => {
            eprint!(
                "{}",
                diagnostic::format_parse_error_ariadne(name, &source, e.line, e.col, &e.message)
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
        let comp = elaborate(&ast, Default::default());
        if !no_typecheck && render_type_errors(&comp) {
            return ExitCode::from(1u8);
        }
        return ExitCode::SUCCESS;
    }

    let mut shell = Shell::new(terminal);
    seed_default_env(&mut shell);
    if let Some(n) = recursion_limit {
        shell.control.recursion_limit = n;
    }
    shell.location.script = name.to_string();
    shell.location.call_site.script = name.to_string();
    shell.location.source = Some(std::sync::Arc::from(source.as_str()));
    shell.dynamic.script_args = script_args;
    builtins::register(&mut shell, baked_prelude_comp());
    tick!("builtins");

    let audit_start = if audit {
        ral_core::types::epoch_us()
    } else {
        0
    };

    if audit {
        shell.audit.tree = Some(Vec::new());
    }

    let comp = elaborate(&ast, Default::default());
    tick!("elaborate");

    if !no_typecheck {
        if render_type_errors(&comp) {
            return ExitCode::from(1u8);
        }
        tick!("typecheck");
    }

    let result = evaluate(&comp, &mut shell);
    tick!("evaluate");

    let tree_children = if audit {
        shell.audit.tree.take().unwrap_or_default()
    } else {
        Vec::new()
    };

    let exit_code = match &result {
        Ok(_) => shell.control.last_status.clamp(0, 255),
        Err(EvalSignal::Exit(code)) => (*code).clamp(0, 255),
        Err(EvalSignal::Error(e)) => {
            if !audit {
                eprint!(
                    "{}",
                    diagnostic::format_runtime_error_auto(
                        name,
                        &source,
                        e,
                        comp.is_single_command(),
                    )
                );
            }
            e.status.clamp(0, 255)
        }
        Err(_) => 1,
    };

    // --audit: emit the execution tree as JSON on stderr.
    if audit {
        emit_audit_tree(name, &result, exit_code, tree_children, audit_start, pretty);
    }

    ExitCode::from(exit_code as u8)
}

// ── main ─────────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    #[cfg(windows)]
    ral_core::compat::enable_virtual_terminal_processing();

    // Hidden multicall dispatch — see `ral_core::builtins::try_run_uutils_helper`
    // for the rationale.  The parent process spawns `current_exe()` with
    // the helper sentinel as the first arg to run a bundled coreutils
    // tool in a fresh subprocess.  Not user-facing; not in `--help`.
    if let Some(code) = ral_core::builtins::uutils::try_run_uutils_helper() {
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
        return code;
    }
    if stripped
        .first()
        .is_some_and(|s| s == "--version" || s == "-V")
    {
        println!("ral {}+{}", env!("CARGO_PKG_VERSION"), env!("RAL_GIT_HASH"));
        return ExitCode::SUCCESS;
    }
    if stripped.first().is_some_and(|s| s == "--help" || s == "-h") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }

    let mode = match parse_args(&stripped) {
        Ok(m) => m,
        Err(e) => {
            diagnostic::cmd_error("ral", &e);
            eprint!("{USAGE}");
            return ExitCode::from(1);
        }
    };

    // Refuse to run setuid — the shell inherits the caller's environment and
    // must not run with elevated privileges the user did not request.
    #[cfg(unix)]
    unsafe {
        if libc::geteuid() != libc::getuid() {
            eprintln!("ral: refusing to run setuid");
            return ExitCode::from(1);
        }
    }

    ral_core::builtins::misc::register_prelude_type_hints(baked_prelude_schemes());

    match mode {
        Mode::Login | Mode::Interactive(_) => {
            let is_login = matches!(mode, Mode::Login);
            let interactive = match &mode {
                Mode::Interactive(o) => *o,
                _ => InteractiveOpts::default(),
            };

            // Non-interactive stdin: read to EOF and execute as a script.
            #[cfg(unix)]
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                use std::io::Read;
                let mut source = String::new();
                if std::io::stdin().read_to_string(&mut source).is_err() {
                    return ExitCode::from(1);
                }
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
            let source = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    diagnostic::cmd_error("ral", &format!("{path}: {e}"));
                    return ExitCode::from(1);
                }
            };
            run_batch(&path, source, script_args, batch)
        }
        Mode::Command {
            code,
            script_args,
            batch,
        } => run_batch("-c", code, script_args, batch),
    }
}

/// True when argv[0] starts with `-`, the POSIX convention indicating
/// that the shell was invoked as a login shell.
fn is_login_shell_argv0() -> bool {
    std::env::args().next().is_some_and(|argv0| {
        std::path::Path::new(&argv0)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.starts_with('-'))
    })
}
