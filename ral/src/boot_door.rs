//! The boot door: `_ral-boot`, bound as the `Session "boot"` hook, is a
//! front-end's first dispatch. It does in a run what must run — startup files
//! evaluate ral — so its `Ending` carries the status: a `--capabilities`
//! failure is `Raised` status 2, a profile's `exit N` is `Exited(N)`. It
//! answers what the rc settled for the host.

use ral_core::protocol::{Ending, Program, Report, Severed};
use ral_core::record;
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum as _;
use ral_core::typecheck::builtins::{closed_record, closed_variant, fun, mk_scheme, pure, thunk};
use ral_core::typecheck::{Row, Scheme, Ty, Unifier};
use ral_core::types::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, Error, HookName, HookSig, Mooring, Settled,
};
use ral_core::{Shell, Value};
use std::borrow::Cow;

use crate::repl::{RcSettings, install_default_prompt, source_startup_files};

const NAME: &str = "_ral-boot";

pub(crate) fn hook() -> HookName {
    HookName::session("boot")
}

static DOOR_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed(NAME),
    scheme,
    "_ral-boot <record>  — boot the session: the front-end's first dispatch, once.",
    BuiltinBody::Static(boot),
)];
pub(crate) static DOOR: &[BuiltinEntry] = &DOOR_ARR;

/// `∀ρ. {ρ} → F settings`: the row stays open, since the door checks its own
/// fields.
fn scheme(u: &mut Unifier) -> Scheme {
    let row = u.fresh_row_var();
    let theme = closed_record(&[
        ("value_prefix", Ty::String),
        (
            "value_color",
            closed_variant(&[("none", Ty::Unit), ("some", Ty::String)]),
        ),
    ]);
    let settings = closed_record(&[
        ("edit_mode", Ty::String),
        ("bell", Ty::Bool),
        ("surface", Ty::String),
        ("theme", theme),
        ("startup", Ty::Bool),
    ]);
    mk_scheme(
        &[],
        &[],
        &[row],
        thunk(fun(Ty::Record(Row::Var(row)), pure(settings))),
    )
}

/// Register the door as the `Session "boot"` hook, over a shell whose surface
/// carries [`DOOR`].
pub(crate) fn register(shell: &mut Shell) -> Result<(), String> {
    let entry = shell
        .lookup_builtin(NAME)
        .ok_or_else(|| format!("this shell's surface has no {NAME}"))?;
    shell
        .register_hook(
            hook(),
            Value::Native {
                entry: entry.into(),
                applied: Vec::new(),
            },
            HookSig::Hook {
                kind: "boot".into(),
            },
            DefaultPolicy::denied(),
            ral_core::source::Span::synthetic(),
        )
        .map_err(|e| e.to_string())
}

/// What a front-end asks of its boot. Batch sources no startup files.
pub(crate) struct Boot {
    pub(crate) login: bool,
    pub(crate) no_rc: bool,
    pub(crate) recursion_limit: Option<usize>,
    pub(crate) capabilities: Vec<String>,
}

record!(Boot {
    login: "login",
    no_rc: "no_rc",
    recursion_limit: "recursion_limit",
    capabilities: "capabilities",
});

impl Boot {
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
    let settings = if boot.no_rc {
        RcSettings::default()
    } else {
        source_startup_files(boot.login, mooring, shell)?
    };
    if let Some(n) = boot.recursion_limit {
        shell.set_stack_limit(n);
    }
    let paths: Vec<std::path::PathBuf> = boot.capabilities.iter().map(Into::into).collect();
    ral_core::capability::apply_session_profiles(mooring, shell, &paths).map_err(|e| match e {
        Break::Error(e) => Break::Error(Error::new(format!("--capabilities: {}", e.message), 2)),
        escape @ Break::Escape(_) => escape,
    })?;
    if shell.is_interactive() {
        install_default_prompt(shell);
    }
    Ok(Value::from(settings.encode()))
}

fn refuse(message: &str) -> Break {
    Break::Error(Error::new(format!("{NAME}: {message}"), 1))
}

/// What the boot dispatch came to: the rc's settings, or the status the
/// session ends with, anything the failure rendered already printed.
pub(crate) fn settle(report: Result<Report, Severed>) -> Result<RcSettings, i32> {
    let ending = match report {
        Err(severed) => {
            eprintln!("ral: {severed}");
            return Err(1);
        }
        Ok(Report::Static { rendered, status }) => {
            eprint!("{rendered}");
            return Err(status);
        }
        Ok(Report::Ran { ending, .. }) => ending,
    };
    let status = ending.status();
    match ending {
        Ending::Settled { value, .. } => RcSettings::decode(&value).map_err(|e| {
            eprintln!("ral: {NAME} answered settings this front-end cannot read: {e}");
            1
        }),
        Ending::Exited(_) => Err(status),
        Ending::Raised { rendered, .. }
        | Ending::Walled { rendered, .. }
        | Ending::Unreturnable { rendered, .. } => {
            eprint!("{rendered}");
            Err(status)
        }
    }
}
