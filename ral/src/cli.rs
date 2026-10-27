//! Command-line parsing and mode selection for the `ral` binary.

use clap::CommandFactory as _;

/// Execution mode derived from argv. Each variant carries exactly the flags
/// valid for it, so misassignment between modes is unrepresentable.
///
/// A login shell with `-c` or a script positional resolves to `Command` or
/// `Script`: the login bit ([`InteractiveOpts::login`]) only means anything in
/// the interactive case.
pub(crate) enum Mode {
    Interactive(InteractiveOpts),
    Script {
        path: std::path::PathBuf,
        script_args: Vec<String>,
        batch: BatchOpts,
    },
    Command {
        code: String,
        script_args: Vec<String>,
        batch: BatchOpts,
    },
}

impl Mode {
    /// Parse argv (without the leading program name) and distil it to a mode.
    pub(crate) fn from_argv(args: &[String]) -> Self {
        use clap::Parser as _;
        Cli::parse_from(std::iter::once("ral").chain(args.iter().map(String::as_str))).into_mode()
    }
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

/// Where a batch run stops short of running: static work on the source alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Halt {
    /// `--dump-ast`: the parse.
    Ast,
    /// `--dump-ir`: the elaborated phrases, whether or not they typecheck.
    Ir,
    /// `--check`: the whole compile, reported as a run reports it.
    Checked,
}

impl Halt {
    fn flag(self) -> &'static str {
        match self {
            Self::Ast => "--dump-ast",
            Self::Ir => "--dump-ir",
            Self::Checked => "--check",
        }
    }
}

/// Flags valid only in batch (script / `-c`) modes.
#[derive(Default, Clone)]
pub(crate) struct BatchOpts {
    pub audit: bool,
    pub pretty: bool,
    pub halt: Option<Halt>,
    pub run: RunOpts,
}

/// Flags valid only in the interactive REPL.
#[derive(Default, Clone)]
// Distinct interactive-mode flags, not a bundle-able group.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct InteractiveOpts {
    /// `-l`, or a `-`-prefixed argv[0]: adopt the POSIX login environment.
    pub login: bool,
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
    #[cfg(unix)]
    pub(crate) fn adopts_login_env(&self) -> bool {
        self.login && !self.no_rc && !self.reads_stdin_as_script()
    }

    pub(crate) fn reads_stdin_as_script(&self) -> bool {
        self.force_stdin || (!self.force_interactive && !stdin_is_terminal())
    }
}

/// Parsed argv surface: `-c` consumes source and arguments; a script positional
/// starts a trailing argument sequence. Clap owns both boundaries.
#[derive(clap::Parser, Debug)]
#[command(
    name = "ral",
    version = concat!(env!("CARGO_PKG_VERSION"), env!("RAL_VERSION_SUFFIX")),
    about = "ral: a typed, structured shell",
    group(clap::ArgGroup::new("halt").args(["check", "dump_ast", "dump_ir"])),
    long_about = "\
ral is a typed, structured shell. Programs pass values: records, lists, \
variants, strings, numbers: and the type checker catches many mistakes before \
execution.

USAGE
    ral                 start an interactive session
    ral <script> [args] run a script
    ral -c <code> [args] run inline code
    ral -s              read a script from stdin
    ral -i              force the interactive REPL

SCRIPT ARGUMENTS
    Arguments after <script> or <code> are available through `args`. Script files \
also bind $SCRIPT to the script path; -c and the REPL leave $SCRIPT unbound.

STARTUP FILES
    Interactive shells read ~/.ralrc or $XDG_CONFIG_HOME/ral/rc. Login shells \
first adopt the environment /bin/sh builds from /etc/profile and ~/.profile. \
Use --norc to skip both.

ENVIRONMENT
    RAL_INTERACTIVE_MODE  line editing: auto or minimal
    RAL_PATH              platform-separated plugin/module search path
    RAL_TIMING            when present, print batch phase timings to stderr

AUDIT
    Use audit { ... } to get a report: how the body settled: `ok of its \
value, or `err of the same error record try hands a handler: beside a trail \
of what it did. The trail lists commands, redirect reads and writes, \
capability denials and worker births, each with its arguments, status, \
output, timing, source location, and principal, in the order they settled. \
Read a report with succeeded and commands. Use --audit to emit the whole \
run's report as JSON.",
)]
#[allow(clippy::struct_excessive_bools)] // clap flag struct: each bool is a distinct CLI switch.
struct Cli {
    /// Start an interactive login shell.
    ///
    /// First adopt the environment /bin/sh builds from /etc/profile and
    /// ~/.profile, then read the rc. Use --norc to skip both.
    /// This option has no effect when ral runs `-c` code, a script file, or a
    /// script from standard input.
    #[arg(long, short = 'l', overrides_with = "login")]
    login: bool,

    /// Write the run's JSON audit report to standard error after the program
    /// finishes.
    ///
    /// The report pairs the run's outcome with a trail of what happened:
    /// commands, redirected reads and writes, capability denials and worker
    /// births, with their statuses, timings, source locations and principals.
    /// It does not repeat the process exit status, which is unchanged. You must
    /// also give ral a script file or use `-c`.
    #[arg(long)]
    audit: bool,

    /// Add indentation and line breaks to the JSON written by `--audit`.
    ///
    /// This makes the report easier to read but larger. You must also use
    /// `--audit`.
    #[arg(long, requires = "audit")]
    pretty: bool,

    /// Check the program without running it.
    ///
    /// Ral parses, elaborates and type-checks the program, then exits. It
    /// returns status 0 when the check succeeds, 1 for a type error and 2 for a
    /// syntax error. You must give ral a script file or use `-c`.
    #[arg(long, short = 'n')]
    check: bool,

    /// Print ral's internal syntax tree to standard error without running it.
    ///
    /// Ral stops after parsing, so this option does not elaborate or type-check
    /// the program. You must give ral a script file or use `-c`.
    #[arg(long = "dump-ast")]
    dump_ast: bool,

    /// Print ral's elaborated phrases to standard error without running them.
    ///
    /// Ral stops after elaboration, so this option does not type-check the
    /// program: it prints the phrases of a program that fails the check too.
    /// You must give ral a script file or use `-c`.
    #[arg(long = "dump-ir")]
    dump_ir: bool,

    /// Set the maximum number of machine frames ral may use while running code.
    ///
    /// N must be a positive integer. The default is 100000. In an interactive
    /// session, this option overrides `recursion-limit` in the rc file. It has
    /// no effect with `--check`, `--dump-ast` or `--dump-ir` because those modes do
    /// not run the program.
    #[arg(long = "recursion-limit", value_name = "N",
          value_parser = clap::value_parser!(u64).range(1..))]
    recursion_limit: Option<u64>,

    /// Restrict the session by loading one or more ral capability profiles.
    ///
    /// Separate paths with commas or repeat this option. Ral combines the
    /// profiles from left to right and applies them before running code. A
    /// relative path names a file in the directory ral starts in, even when a
    /// start-up file changes directory. It has
    /// no effect with `--check`, `--dump-ast` or `--dump-ir` because those modes do
    /// not start a runtime session.
    #[arg(long, value_name = "PATHS", value_delimiter = ',',
          action = clap::ArgAction::Append)]
    capabilities: Vec<std::path::PathBuf>,

    /// Run the next argument as ral source code.
    ///
    /// Ral puts every later argument in `args`, even when it starts with a
    /// hyphen. For example, `ral -c 'show !{args}' one two` runs the given source
    /// with `one` and `two` as its arguments. Put shell options before `-c`;
    /// in short clusters, put `c` last (`-ec`, `-lc`). Text attached after `c`
    /// is source, so `-ce` runs `e`. Inline code does not bind `$SCRIPT`.
    #[arg(short = 'c', num_args = 1.., allow_hyphen_values = true, value_name = "CODE_OR_ARG")]
    code: Vec<String>,

    /// Start the interactive REPL even when standard input is not a terminal.
    ///
    /// This is useful when another program supplies ral's input. `-s` takes
    /// priority over this option. A script path or `-c` also takes priority.
    #[arg(short = 'i', overrides_with = "force_interactive")]
    force_interactive: bool,

    /// Read and run a script from standard input, even when it is a terminal.
    ///
    /// This option takes priority over `-i`. A script path or `-c` takes
    /// priority over both options. A script read from standard input does not
    /// bind `$SCRIPT`.
    #[arg(short = 's', overrides_with = "force_stdin")]
    force_stdin: bool,

    /// Accept `-e` from programs that expect a POSIX shell; do nothing.
    #[arg(short = 'e', hide = true, overrides_with = "posix_e")]
    posix_e: bool,

    /// Accept `-u` from programs that expect a POSIX shell; do nothing.
    #[arg(short = 'u', hide = true, overrides_with = "posix_u")]
    posix_u: bool,

    /// Do not read interactive start-up files.
    ///
    /// Ral skips the rc file and, for a login shell, adoption of the environment
    /// /bin/sh builds from /etc/profile and ~/.profile. This only affects an
    /// interactive session. You can also spell it `--noprofile`.
    #[arg(long, visible_alias = "noprofile", overrides_with = "norc")]
    norc: bool,

    /// Choose the interface for an interactive session.
    ///
    /// This option overrides `surface` in the rc file. The choices are
    /// `readline` (the default) and `minimal`. A terminal resolved to minimal
    /// mode still uses the minimal interface.
    #[arg(long, value_enum, value_name = "SURFACE")]
    surface: Option<crate::repl::Surface>,

    /// Give ral a script followed by its arguments.
    ///
    /// The first ARG is the script path and the remaining values become
    /// `args`; ral also binds `$SCRIPT` to the path. Use `--` before a script
    /// path that starts with a hyphen, except the literal filename `-`.
    #[arg(trailing_var_arg = true, value_name = "ARG")]
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
    fn into_mode(self) -> Mode {
        let login = self.login || is_login_shell_argv0();

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
        let halt = [
            (self.check, Halt::Checked),
            (self.dump_ast, Halt::Ast),
            (self.dump_ir, Halt::Ir),
        ]
        .into_iter()
        .find_map(|(on, halt)| on.then_some(halt));
        let batch = BatchOpts {
            audit: self.audit,
            pretty: self.pretty,
            halt,
            run: run.clone(),
        };

        if !self.code.is_empty() {
            let mut it = self.code.into_iter().chain(self.rest);
            let code = it.next().expect("clap requires source after -c");
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
                path: path.into(),
                script_args: it.collect(),
                batch,
            };
        }

        reject_batch_flags_without_batch(self.audit, halt);

        Mode::Interactive(InteractiveOpts {
            login,
            no_rc: self.norc,
            force_interactive: self.force_interactive,
            force_stdin: self.force_stdin,
            surface: self.surface,
            run,
        })
    }
}

fn reject_batch_flags_without_batch(audit: bool, halt: Option<Halt>) {
    let Some(flag) = audit.then_some("--audit").or_else(|| halt.map(Halt::flag)) else {
        return;
    };
    Cli::command()
        .error(
            clap::error::ErrorKind::MissingRequiredArgument,
            format!("{flag} requires a script or -c"),
        )
        .exit();
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

    fn mode_of(args: &[&str]) -> Mode {
        let raw: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
        Mode::from_argv(&raw)
    }

    #[test]
    fn compatibility_flags_may_repeat() {
        for args in [&["-l", "-l"][..], &["-il", "-l"]] {
            let Mode::Interactive(opts) = mode_of(args) else {
                panic!("interactive");
            };
            assert!(opts.login);
        }
        let Mode::Interactive(opts) = mode_of(&["--norc", "--noprofile"]) else {
            panic!("interactive");
        };
        assert!(opts.no_rc);
    }

    #[test]
    fn login_mode_parse_matrix() {
        match mode_of(&["-l"]) {
            Mode::Interactive(o) => assert!(o.login && !o.no_rc),
            m => panic!("`-l` should be Interactive, got {}", mode_name(&m)),
        }

        match mode_of(&["-lc", "echo hi"]) {
            Mode::Command { code, .. } => assert_eq!(code, "echo hi"),
            m => panic!("`-lc 'echo hi'` should be Command, got {}", mode_name(&m)),
        }

        match mode_of(&["-l", "--norc"]) {
            Mode::Interactive(o) => {
                assert!(o.login);
                assert!(o.no_rc, "`-l --norc` must keep no_rc");
            }
            m => panic!("`-l --norc` should be Interactive, got {}", mode_name(&m)),
        }

        match mode_of(&["-l", "script.ral"]) {
            Mode::Script { path, .. } => assert_eq!(path, std::path::PathBuf::from("script.ral")),
            m => panic!("`-l script.ral` should be Script, got {}", mode_name(&m)),
        }
    }

    #[test]
    fn non_login_mode_parse_matrix() {
        match mode_of(&[]) {
            Mode::Interactive(o) => assert!(!o.login),
            m => panic!("no arguments should be Interactive, got {}", mode_name(&m)),
        }
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
                assert_eq!(path, std::path::PathBuf::from("script.ral"));
                assert_eq!(script_args, vec!["arg1".to_string()]);
            }
            m => panic!("`script.ral arg1` should be Script, got {}", mode_name(&m)),
        }
    }

    fn mode_name(m: &Mode) -> &'static str {
        match m {
            Mode::Interactive(_) => "Interactive",
            Mode::Script { .. } => "Script",
            Mode::Command { .. } => "Command",
        }
    }

    #[test]
    fn code_is_a_value_taking_option() {
        use clap::Parser as _;

        let cli = Cli::try_parse_from(["ral", "-ce", "extra"]).unwrap();
        assert_eq!(cli.code, ["e"]);
        assert!(!cli.posix_e);
        match cli.into_mode() {
            Mode::Command {
                code, script_args, ..
            } => {
                assert_eq!(code, "e");
                assert_eq!(script_args, ["extra"]);
            }
            m => panic!("expected Command, got {}", mode_name(&m)),
        }
        for args in [&["ral", "-c"][..], &["ral", "--unknown"]] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn value_taking_long_options_keep_their_values() {
        match mode_of(&["--recursion-limit", "2048", "script.ral", "-n"]) {
            Mode::Script {
                path,
                script_args,
                batch,
            } => {
                assert_eq!(path, std::path::PathBuf::from("script.ral"));
                assert_eq!(script_args, ["-n"]);
                assert_eq!(batch.run.recursion_limit, Some(2048));
                assert_eq!(batch.halt, None);
            }
            m => panic!("expected Script, got {}", mode_name(&m)),
        }
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
