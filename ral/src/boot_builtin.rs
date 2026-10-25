//! The boot builtin: `_ral-boot`, bound as the `Session "boot"` hook, is a
//! front-end's first dispatch. It does in a run what must run — startup files
//! evaluate ral — so its `Ending` carries the status: a `--capabilities`
//! failure is `Raised` status 2, a profile's `exit N` is `Exited(N)`. It
//! answers what the rc settled for the host.

use ral_core::carrier::Severed;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::host::EnvVars;
use ral_core::path::{Resolver, is_absolute, shown, utf8};
use ral_core::protocol::{Ending, Program, Report};
use ral_core::record;
use ral_core::shell::runtime::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, HookName, HookSig, Mooring, Settled, Status,
    sig,
};
use ral_core::ty::{Row, Scheme, Ty, Typed as _};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{fun, mk_scheme, pure, thunk};
use ral_core::{Shell, Value};
use ral_core::{err, errln};
use std::borrow::Cow;
use std::path::PathBuf;

use crate::repl::{RcSettings, install_default_prompt, source_startup_files};

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

/// `∀ρ. {ρ} → F settings`: the row stays open, since the builtin checks its own
/// fields.
fn scheme(u: &mut Unifier) -> Scheme {
    let row = u.fresh_row_var();
    mk_scheme(
        &[],
        &[(row, false)],
        thunk(fun(Ty::Record(Row::Var(row)), pure(RcSettings::ty()))),
    )
}

/// Register the builtin as the `Session "boot"` hook, over a shell whose surface
/// carries [`BUILTINS`].
pub(crate) fn register(shell: &mut Shell) -> Result<(), String> {
    let entry = shell
        .lookup_builtin(NAME)
        .ok_or_else(|| format!("this shell's surface has no {NAME}"))?;
    shell
        .register_hook(
            hook(),
            Value::Native {
                entry: entry.into(),
                applied: Box::new([]),
            },
            HookSig::Hook {
                kind: "boot".into(),
            },
            DefaultPolicy::denied(),
        )
        .map_err(|e| e.to_string())
}

/// What a front-end asks of its boot. Batch sources no startup files.
pub(crate) struct Boot {
    pub(crate) login: bool,
    pub(crate) no_rc: bool,
    pub(crate) recursion_limit: Option<usize>,
    /// The `--capabilities` profiles, each the native spelling of an
    /// absolute path ([`Self::profiles`]).
    pub(crate) capabilities: Vec<String>,
}

record!(Boot {
    login: "login",
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
        source_startup_files(boot.login, mooring, shell)?
    };
    if let Some(n) = boot.recursion_limit {
        shell.set_stack_limit(n);
    }
    let paths: Vec<PathBuf> = boot.capabilities.iter().map(Into::into).collect();
    ral_core::shell::load::profile::apply_session_profiles(mooring, shell, &paths).map_err(
        |e| match e {
            Break::Error(mut e) => {
                e.status = Status::Raised(2);
                Break::Error(e.context("--capabilities"))
            }
            escape @ Break::Escape(_) => escape,
        },
    )?;
    if shell.is_interactive() {
        install_default_prompt(shell);
    }
    Ok(Value::from(settings.encode()))
}

fn refuse(message: &str) -> Break {
    sig(format!("{NAME}: {message}"))
}

/// What the boot dispatch came to: the rc's settings, or the status the
/// session ends with, anything the failure rendered already printed.
pub(crate) fn settle(report: Result<Report, Severed>) -> Result<RcSettings, i32> {
    let ending = match report {
        Err(severed) => {
            errln!("ral: {severed}");
            return Err(1);
        }
        Ok(Report::Static { rendered, status } | Report::Faulted { rendered, status }) => {
            err!("{rendered}");
            return Err(status.get());
        }
        Ok(Report::Ran { ending, .. }) => ending,
    };
    let status = ending.status();
    match ending {
        Ending::Settled { value, .. } => RcSettings::decode(&value).map_err(|e| {
            errln!("ral: {NAME} answered settings this front-end cannot read: {e}");
            1
        }),
        Ending::Exited(_) => Err(status),
        Ending::Raised { rendered, .. }
        | Ending::Stopped { rendered, .. }
        | Ending::Unreturnable { rendered, .. } => {
            err!("{rendered}");
            Err(status)
        }
    }
}
