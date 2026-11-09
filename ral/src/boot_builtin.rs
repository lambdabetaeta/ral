//! The boot builtin: `_ral-boot`, bound as the `Session "boot"` hook, is a
//! front-end's first dispatch. It does in a run what must run — startup files
//! evaluate ral. Capability refusals are data whose host exit is 2;
//! an rc failure remains an error ending.

use ral_core::carrier::Severed;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::environ::EnvVars;
use ral_core::error::Error;
use ral_core::path::{Resolver, is_absolute, shown, utf8};
use ral_core::protocol::{Ending, Program, Ran, Report};
use ral_core::record;
use ral_core::shell::runtime::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, HookName, HookSig, Mooring, Settled,
};
use ral_core::ty::{Row, Scheme, Ty, Typed as _};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{fun, mk_scheme, pure, thunk};
use ral_core::{RegisterError, Shell, Value};
use ral_core::{err, errln};
use std::borrow::Cow;
use std::path::PathBuf;

use crate::repl::{RcSettings, install_default_prompt, source_rc_file};

const NAME: &str = "_ral-boot";

pub(crate) fn hook() -> HookName {
    HookName::session("boot")
}

static BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed(NAME),
    scheme,
    "_ral-boot <record>  — boot the session: the front-end's first dispatch, once.",
    BuiltinBody::Static(boot),
)];
pub(crate) static BUILTINS: &[BuiltinEntry] = &BUILTINS_ARR;

/// The request row stays open; the answer distinguishes host refusals.
fn scheme(u: &mut Unifier) -> Scheme {
    let row = u.fresh_row_var();
    mk_scheme(
        &[],
        &[(row, false)],
        thunk(fun(Ty::Record(Row::Var(row)), pure(BootAnswer::ty()))),
    )
}

/// Register the builtin as the `Session "boot"` hook, over a shell whose surface
/// carries [`BUILTINS`].
pub(crate) fn register(shell: &mut Shell) -> Result<(), Error> {
    let entry = shell
        .lookup_builtin(NAME)
        .ok_or_else(|| Error::new(format!("this shell's surface has no {NAME}")))?;
    shell
        .register_hook(
            hook(),
            Value::Native(entry.into()),
            HookSig::Hook {
                kind: "boot".into(),
            },
            DefaultPolicy::denied(),
        )
        .map_err(RegisterError::into_error)
}

enum BootAnswer {
    Settled(RcSettings),
    CapabilitiesRefused(String),
}

ral_core::variant!(typed BootAnswer {
    Settled(RcSettings): "settled",
    CapabilitiesRefused(String): "capabilities-refused",
});

/// What a front-end asks of its boot. Batch sources no startup files.
pub(crate) struct Boot {
    pub(crate) no_rc: bool,
    pub(crate) recursion_limit: Option<usize>,
    /// The `--capabilities` profiles, each the native spelling of an
    /// absolute path ([`Self::profiles`]).
    pub(crate) capabilities: Vec<String>,
}

record!(Boot {
    no_rc: "no-rc",
    recursion_limit: "recursion-limit",
    capabilities: "capabilities",
});

impl Boot {
    /// The `--capabilities` profiles as the record carries them: each path
    /// anchored at the invocation cwd and spelled natively, which the builtin
    /// lifts back unchanged.
    ///
    /// A relative argument names a file beside the user who typed it, and the
    /// rc and earlier profiles run before a profile loads and may `cd`, so the
    /// argument is anchored here, before any of them runs: the shell's cwd at
    /// load time is not the one the user meant.  The invocation cwd is the
    /// process's own, which ral never moves.  The record is text, so a path
    /// that is not UTF-8 cannot board it.
    ///
    /// # Errors
    /// The first path that is not UTF-8, or that is relative while the
    /// invocation cwd is unknown, named.
    pub(crate) fn profiles(paths: &[PathBuf]) -> Result<Vec<String>, String> {
        let cwd = ral_core::host::cwd();
        paths
            .iter()
            .map(|path| {
                let anchored = match &cwd {
                    Some(cwd) => Resolver::at(&EnvVars::new(), cwd).anchor(path).into_inner(),
                    None if is_absolute(path) => path.clone(),
                    None => {
                        return Err(format!(
                            "ral: --capabilities: {} cannot be anchored: the working \
                             directory is unavailable",
                            shown(path)
                        ));
                    }
                };
                utf8(&anchored)
                    .map(str::to_owned)
                    .map_err(|e| format!("ral: --capabilities: {e}, so it cannot name a profile"))
            })
            .collect()
    }

    pub(crate) fn program(self) -> Program {
        Program::Hook {
            name: hook(),
            args: vec![self.encode()],
        }
    }
}

/// Spent on first use: the hook goes, so a second call finds none. The CLI's
/// overrides land after the rc, and `--capabilities` narrows from there: rc
/// is operator-trusted bootstrap, the user's ceiling is the last word.
fn boot(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    if !shell.unregister_hook(&hook()) {
        return Err(refuse("the session has already booted"));
    }
    let record = args
        .first()
        .and_then(|v| FOValue::try_from(v).ok())
        .ok_or_else(|| refuse("the boot record must be first-order data"))?;
    let boot = Boot::decode(&record).map_err(|e| refuse(&format!("in the boot record, {e}")))?;
    // The front-end anchored every profile (`Boot::profiles`); a relative
    // one would now read the shell's cwd, which the rc may yet move.
    if let Some(path) = boot.capabilities.iter().find(|p| !is_absolute(p)) {
        return Err(refuse(&format!(
            "the --capabilities profile {} is not absolute",
            shown(path)
        )));
    }
    let settings = if boot.no_rc {
        RcSettings::default()
    } else {
        source_rc_file(mooring, shell)?
    };
    if let Some(n) = boot.recursion_limit {
        shell.set_stack_limit(n);
    }
    let paths: Vec<PathBuf> = boot.capabilities.iter().map(Into::into).collect();
    match ral_core::shell::load::profile::apply_session_profiles(mooring, shell, &paths) {
        Ok(()) => {}
        Err(Break::Error(e)) => {
            let diagnostic =
                e.with_context("--capabilities")
                    .render(shell.sources(), None, shell.exit_hints());
            return Ok(Value::from(
                BootAnswer::CapabilitiesRefused(diagnostic).encode(),
            ));
        }
        Err(escape @ Break::Escape(_)) => return Err(escape),
    }
    if shell.is_interactive() {
        install_default_prompt(shell);
    }
    Ok(Value::from(BootAnswer::Settled(settings).encode()))
}

fn refuse(message: &str) -> Break {
    Error::new(format!("{NAME}: {message}")).into()
}

/// Decode the boot answer; only the host chooses capability refusals' exit 2.
pub(crate) fn settle(report: Result<Report, Severed>) -> Result<RcSettings, u8> {
    let report = match report {
        Ok(report) => report,
        Err(severed) => {
            errln!("ral: {severed}");
            return Err(1);
        }
    };
    let code = report.code();
    match report {
        Report::Ran(Ran {
            ending: Ending::Ok(value),
            ..
        }) => match BootAnswer::decode(&value) {
            Ok(BootAnswer::Settled(settings)) => Ok(settings),
            Ok(BootAnswer::CapabilitiesRefused(diagnostic)) => {
                err!("{diagnostic}");
                Err(2)
            }
            Err(e) => {
                errln!("ral: {NAME} answered a boot result this front-end cannot read: {e}");
                Err(1)
            }
        },
        Report::Ran(Ran {
            ending: Ending::Exited(_),
            ..
        }) => Err(code),
        Report::Static { rendered, .. }
        | Report::Faulted { rendered, .. }
        | Report::Ran(Ran { rendered, .. }) => {
            err!("{rendered}");
            Err(code)
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn report(answer: BootAnswer) -> Report {
        Report::Ran(Ran {
            ending: Ending::Ok(answer.encode()),
            rendered: String::new(),
            single_command: false,
            captured: None,
            trail: Vec::new(),
        })
    }

    #[test]
    fn settle_decodes_the_boot_sum_and_chooses_two_only_for_capability_refusals() {
        assert!(settle(Ok(report(BootAnswer::Settled(RcSettings::default())))).is_ok());
        assert!(matches!(
            settle(Ok(report(BootAnswer::CapabilitiesRefused(
                "refused\n".into()
            )))),
            Err(2)
        ));
        let report = Report::Ran(Ran {
            ending: Ending::Err(ral_core::fact::ErrorRecord::new(
                ral_core::fact::Reason::Raised("rc failed".into()),
                "fail".into(),
                ral_core::source::CallSite {
                    script: "rc.ral".into(),
                    line: 1,
                    col: 1,
                },
            )),
            rendered: "rc failed\n".into(),
            single_command: false,
            captured: None,
            trail: Vec::new(),
        });
        assert!(matches!(settle(Ok(report)), Err(1)));
    }

    #[test]
    fn boot_renders_a_capability_profiles_compile_rejection_engine_side() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("broken.ral");
        std::fs::write(&profile, "let [").unwrap();
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_builtins(BUILTINS);
        let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        register(&mut shell).unwrap();
        let request = Boot {
            no_rc: true,
            recursion_limit: None,
            capabilities: vec![utf8(&profile).unwrap().to_owned()],
        };
        let answer = boot(
            &[Value::from(request.encode())],
            &Mooring::adrift(),
            &mut shell,
        )
        .unwrap();
        let BootAnswer::CapabilitiesRefused(diagnostic) =
            BootAnswer::decode(&FOValue::try_from(&answer).unwrap()).unwrap()
        else {
            panic!("a rejected capability profile is a boot answer, not an rc failure")
        };
        assert!(diagnostic.contains("--capabilities"), "{diagnostic}");
        assert!(
            diagnostic.contains("broken.ral:1:5") && diagnostic.contains("[L0002]"),
            "{diagnostic}"
        );
        for detail in [
            "unterminated `[…]`",
            "`[` opened here",
            "expected closing `]` before end of input",
        ] {
            assert!(diagnostic.contains(detail), "{diagnostic}");
        }
    }
}
