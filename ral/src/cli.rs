//! Command-line parsing and mode selection for the `ral` binary.

use clap::CommandFactory as _;

/// Execution mode derived from argv. Each variant carries exactly the flags
/// valid for it, so misassignment between modes is unrepresentable.
///
/// `Login` is the interactive REPL with login-profile sourcing; it carries the
/// same [`InteractiveOpts`] as `Interactive` so `--norc` and the rest survive.
/// A login shell with `-c` or a script positional resolves to `Command` or
/// `Script` instead: the login bit only distinguishes the interactive case.
pub(crate) enum Mode {
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
    /// `--recursion-limit N` overrides the rc default and the built-in.
    pub recursion_limit: Option<usize>,
    /// `--capabilities a.ral,b.ral[,c.ral]` loads capability profiles at
    /// session start. Multiple files compose left-to-right by `meet`.
    pub capabilities: Vec<std::path::PathBuf>,
}

/// Flags valid only in batch (script / `-c`) modes.
#[derive(Default, Clone)]
// Distinct batch-mode flags, not a bundle-able group.
#[allow(clippy::struct_excessive_bools)]
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
    /// `-i` forces interactive mode even when stdin is not a tty.
    pub force_interactive: bool,
    /// `-s` reads stdin as a batch script even when stdin is a tty. Consulted
    /// only in the interactive case: a script positional, if present, is run
    /// instead. Takes precedence over `-i` (`force_interactive`).
    pub force_stdin: bool,
    /// `--surface` selects the interactive frontend. `None` leaves the choice
    /// to the rc `surface:` key, falling back to the default surface.
    pub surface: Option<crate::repl::Surface>,
    pub run: RunOpts,
}

impl InteractiveOpts {
    pub(crate) fn reads_stdin_as_script(&self) -> bool {
        self.force_stdin || (!self.force_interactive && !stdin_is_terminal())
    }
}

/// Parsed argv surface, built by clap.
///
/// `-c` is a bool flag rather than a value-taking flag so that everything after
/// it (the inline code and any trailing positionals) is captured verbatim by
/// `rest` via `trailing_var_arg`. This mirrors the lexopt behaviour exactly:
/// once `-c` is seen, the remainder is slurped without further flag parsing,
/// even if items look like flags.
#[derive(clap::Parser, Debug)]
#[command(
    name = "ral",
    version = concat!(env!("CARGO_PKG_VERSION"), env!("RAL_VERSION_SUFFIX")),
    about = "ral — a typed, structured shell",
    long_about = "\
ral is a typed, structured shell. Programs pass values — records, lists, \
variants, strings, numbers — and the type checker catches many mistakes before \
execution.

USAGE
    ral                 start an interactive session
    ral <script> [args] run a script
    ral -c <code> [args] run inline code
    ral -s              read a script from stdin
    ral -i              force the interactive REPL

SCRIPT ARGUMENTS
    Arguments after <script> or <code> are available as $args. Script files \
also bind $script to the script path; -c and the REPL leave $script unbound.

STARTUP FILES
    Interactive shells read ~/.ralrc or $XDG_CONFIG_HOME/ral/rc. Login shells \
also read ~/.ral_profile and /etc/ral/profile. Use --norc to skip startup \
files.

ENVIRONMENT
    RAL_INTERACTIVE_MODE  line editing: auto or minimal
    RAL_PATH              platform-separated plugin/module search path
    RAL_TIMING            when present, print batch phase timings to stderr

AUDIT
    Use audit { ... } inside a script to record the exact argv and environment \
handed to execve(2) for external commands. Use --audit to emit the execution \
tree as JSON.",
)]
#[allow(clippy::struct_excessive_bools)] // clap flag struct: each bool is a distinct CLI switch.
pub(crate) struct Cli {
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

    /// Maximum function-call recursion depth (default 1024; overrides rc `recursion_limit`:)
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

    /// Read stdin as a script even when stdin is a terminal (a script positional takes precedence)
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
    /// The login bit (`-l` or a `-`-prefixed `argv[0]`) does not short-circuit: a
    /// login shell invoked with `-c` or a script positional must still run that
    /// command rather than dropping it for an interactive REPL. Login therefore
    /// only selects between the two interactive variants, decided after
    /// `-c`/script are ruled out.
    pub(crate) fn into_mode(self) -> Mode {
        let is_login = self.login || is_login_shell_argv0();

        let capabilities = self
            .capabilities
            .into_iter()
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        let run = RunOpts {
            recursion_limit: self.recursion_limit.map(|n| {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "recursion limit; 64-bit target where usize == u64"
                )]
                let n = n as usize;
                n
            }),
            capabilities,
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
                Self::command()
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
            let path = it.next().expect("rest is non-empty");
            return Mode::Script {
                path,
                script_args: it.collect(),
                batch,
            };
        }

        reject_batch_flags_without_batch(self.audit, self.check, self.dump_ast);

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

/// Insert `--` so that clap's `last = true` semantics on `rest` capture the
/// script/`-c` remainder uniformly, even when it is flag-shaped.
///
/// Two cases inject the terminator:
/// - immediately after a `-c`, so `ral -c '--version'` runs the code instead
///   of clap reading `--version` as the version flag;
/// - before the first non-option positional, so script arguments are not
///   re-parsed as ral's own flags.
///
/// Long flags that take a separate value token carry that value past the flag.
/// Which flags those are is read from clap's own model ([`value_taking_longs`])
/// rather than hand-listed, so it cannot drift from the `Cli` definition.
pub(crate) fn inject_arg_terminator(raw: &[String]) -> Vec<String> {
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
            if is_code_flag(arg) {
                out.push("--".to_string());
                out.extend(raw[i..].iter().cloned());
                return out;
            }
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

fn reject_batch_flags_without_batch(audit: bool, check: bool, dump_ast: bool) {
    if !audit && !check && !dump_ast {
        return;
    }

    let flag = if audit {
        "--audit"
    } else if check {
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

/// The `--long` flags that take a separate value token, read from clap's own
/// argument model so [`inject_arg_terminator`] can never disagree with `Cli`.
fn value_taking_longs() -> Vec<String> {
    Cli::command()
        .get_arguments()
        .filter(|arg| arg.get_action().takes_values())
        .filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
        .collect()
}

/// Whether `arg` is the `-c` inline-code flag, alone or as the trailing letter
/// of a single-dash short cluster (`-lc`). In a cluster the `c` must be last,
/// since the following token is the code it consumes.
fn is_code_flag(arg: &str) -> bool {
    arg.strip_prefix('-')
        .filter(|rest| !rest.starts_with('-') && rest.chars().all(|c| c.is_ascii_alphabetic()))
        .is_some_and(|rest| rest.ends_with('c'))
}

/// True when `argv[0]` starts with `-`, the POSIX convention indicating that the
/// shell was invoked as a login shell.
fn is_login_shell_argv0() -> bool {
    std::env::args()
        .next()
        .is_some_and(|argv0| ral_core::path::basename(&argv0).starts_with('-'))
}

#[cfg(unix)]
fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal()
}

#[cfg(not(unix))]
fn stdin_is_terminal() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// Parse argv (without the leading program name) the same way `main` does:
    /// terminator injection, then clap, then distil to a [`Mode`].
    fn mode_of(args: &[&str]) -> Mode {
        let raw: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
        Cli::parse_from(std::iter::once("ral".to_string()).chain(inject_arg_terminator(&raw)))
            .into_mode()
    }

    #[test]
    fn login_mode_parse_matrix() {
        match mode_of(&["-l"]) {
            Mode::Login(o) => assert!(!o.no_rc),
            m => panic!("`-l` should be Login, got {}", mode_name(&m)),
        }

        match mode_of(&["-lc", "echo hi"]) {
            Mode::Command { code, .. } => assert_eq!(code, "echo hi"),
            m => panic!("`-lc 'echo hi'` should be Command, got {}", mode_name(&m)),
        }

        match mode_of(&["-l", "--norc"]) {
            Mode::Login(o) => assert!(o.no_rc, "`-l --norc` must keep no_rc"),
            m => panic!("`-l --norc` should be Login, got {}", mode_name(&m)),
        }

        match mode_of(&["-l", "script.ral"]) {
            Mode::Script { path, .. } => assert_eq!(path, "script.ral"),
            m => panic!("`-l script.ral` should be Script, got {}", mode_name(&m)),
        }
    }

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
        let raw: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
        inject_arg_terminator(&raw)
    }

    #[test]
    fn arg_terminator_slurps_code_after_dash_c() {
        assert_eq!(inject(&["-c", "--version"]), vec!["-c", "--", "--version"]);
        assert_eq!(inject(&["-lc", "echo hi"]), vec!["-lc", "--", "echo hi"]);
        assert_eq!(
            inject(&["-c", "echo hi", "-n"]),
            vec!["-c", "--", "echo hi", "-n"]
        );
    }

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
        assert_eq!(
            inject(&["-l", "script.ral"]),
            vec!["-l", "--", "script.ral"]
        );
    }

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
