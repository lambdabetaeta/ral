//! The in-process guards.
//!
//! Every runtime yes/no asked as an action is attempted, exec and its argv,
//! an fs read or write, a boolean flag, folds the whole dynamic
//! [`GrantStack`], so a verdict is authority intersected across every layer,
//! never one frame.  The judgments are the model's ([`GrantStack::admit`],
//! [`GrantStack::read`]); this module words their refusals and, for the exec and fs
//! guards, which reach a real OS resource, builds the [`Denial`] the shell
//! records.  The sandbox projects the same authority off the same folds,
//! so the two cannot drift.

use crate::authority::policy::{
    Admitted, Baseline, ExecDenial, ExecRefused, Flag, FsDeny, FsOp, GrantStack, LocateError,
    Program, Refused, ShellVerb, Withheld,
};
use crate::fact::{Check, Resource};
use crate::path::shown;
use crate::path::{LexicalPath, MatchedBy, RealPath, Trace};
use crate::text::code;
use std::fmt::{self, Display};

/// The guard's diagnostic; the shell gives it runtime status and control flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub message: String,
    pub hint: Option<String>,
}

impl Refusal {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// A refused exec or fs check: the fact the trail keeps, and the refusal the
/// caller raises.  An admitted check is never recorded: a trail of every
/// permitted read says nothing an auditor asked.
pub(crate) struct Denial {
    pub check: Check,
    pub error: Refusal,
}

impl Denial {
    fn new(
        resource: Resource,
        fields: impl IntoIterator<Item = (&'static str, String)>,
        error: Refusal,
    ) -> Self {
        let fields = fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        Self {
            check: Check::new(resource, fields),
            error,
        }
    }
}

/// What an enclosing grant may do to a missing allowance, said only where
/// more than one layer supplies the axis.
const NESTED: &str = "an enclosing grant may be the one that withholds it, and a nested grant \
                      cannot add authority its caller withholds";

/// What an exclusion says of its subject, which no allowance overrides.
fn excluded_tail(scope: impl Display, matched_by: MatchedBy) -> String {
    match matched_by {
        MatchedBy::Name => {
            format!("is excluded by {scope}; an allowance cannot override an exclusion")
        }
        MatchedBy::FoldedName => format!(
            "is the denied {scope} under another spelling \
             (case or Unicode form); a deny holds under every spelling"
        ),
        MatchedBy::Identity => format!("is the denied object {scope} under another name"),
    }
}

/// The exclusion half of a refusal, which no allowance overrides.
fn excluded(subject: impl Display, scope: impl Display, matched_by: MatchedBy) -> String {
    format!("{subject} {}", excluded_tail(scope, matched_by))
}

/// What a refusal says of its object, which the caller names ahead of it.
impl Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Guarded(pinned) => write!(
                f,
                "it is the {pinned} binary ral pinned at startup, which ral never writes into"
            ),
            Self::Withheld(Withheld::MissingAllowance) => {
                f.write_str("no allowance of the active grant covers it")
            }
            Self::Withheld(Withheld::Excluded { scope, matched_by }) => {
                write!(f, "it {}", excluded_tail(scope, *matched_by))
            }
        }
    }
}

impl Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Refused(refused) => refused.fmt(f),
        }
    }
}

const EXCLUSION_HINT: &str =
    "the exclusion belongs to the policy that imposes it; change that policy";

#[derive(Clone, Copy)]
enum Refusing<'a> {
    Program(&'a Program),
    File(&'a RealPath),
}

impl Display for Refusing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Program(program) => program.fmt(f),
            Self::File(real) => real.fmt(f),
        }
    }
}

/// Judge a program: the one exec door.
pub(crate) fn check_exec(
    grants: &GrantStack,
    baseline: &Baseline,
    shown: &str,
    program: Program,
) -> Result<Admitted, Denial> {
    grants.admit(baseline, program).map_err(|refused| {
        let ExecRefused { program, why } = *refused;
        let program = Refusing::Program(&program);
        let error = exec_error(grants, baseline, shown, program, &why);
        exec_denial(shown, program, &why, error)
    })
}

/// Judge a name a head's walk reached no object at, as running it would be.
pub(crate) fn check_exec_name(
    grants: &GrantStack,
    baseline: &Baseline,
    shown: &str,
    trace: &Trace,
) -> Result<(), Denial> {
    grants.admit_name(baseline, trace).map_err(|why| {
        let error = exec_error(grants, baseline, shown, Refusing::File(trace.real()), &why);
        exec_denial(shown, Refusing::File(trace.real()), &why, error)
    })
}

/// The words of an exec refusal.
fn exec_error(
    grants: &GrantStack,
    baseline: &Baseline,
    shown: &str,
    program: Refusing<'_>,
    why: &ExecDenial,
) -> Refusal {
    let layers = grants.exec().count();
    let head = format!("command '{shown}' denied by active grant");
    match why {
        ExecDenial::Run(Withheld::MissingAllowance) if layers > 1 => Refusal::new(format!(
            "{head}: some layer's exec `allow` list does not let it run"
        ))
        .with_hint(NESTED),
        ExecDenial::Run(Withheld::MissingAllowance) => Refusal::new(format!(
            "{head}: nothing in the grant's exec `allow` list lets it run"
        ))
        .with_hint(format!(
            "add it to the grant's exec `allow` list: {} for a command name, {} for a file, \
                 or {} for its directory",
            code("`name git"),
            code("`absolute /path/to/it"),
            code("`dir `absolute /path"),
        )),
        ExecDenial::Run(Withheld::Excluded { scope, matched_by }) => Refusal::new(format!(
            "{head}: {}",
            excluded(
                format_args!("'{program}'"),
                code(&scope.to_string()),
                *matched_by
            )
        ))
        .with_hint(EXCLUSION_HINT),
        ExecDenial::Read(why) => Refusal::new(format!(
            "{head}: {}",
            read_cause(baseline, shown, program, why)
        ))
        .with_hint(read_hint(why, grants.fs().count())),
    }
}

/// Why running `program` is refused for what it reads: a host file, or for a
/// bundled tool ral's binary, which it runs.
fn read_cause(
    baseline: &Baseline,
    shown: &str,
    program: Refusing<'_>,
    why: &Withheld<FsDeny>,
) -> String {
    let (lead, subject) = match program {
        Refusing::Program(Program::Tool(_)) => (
            format!("'{shown}' is built into ral, and "),
            baseline.image().map_or_else(
                || "ral's binary".to_string(),
                |own| format!("ral's binary '{}'", own.real()),
            ),
        ),
        file => (String::new(), format!("'{file}'")),
    };
    match why {
        Withheld::MissingAllowance => {
            format!("{lead}{subject} may run, but it is not readable under the active grant")
        }
        Withheld::Excluded { scope, matched_by } => format!(
            "{lead}{}",
            excluded(subject, format_args!("the fs deny '{scope}'"), *matched_by)
        ),
    }
}

fn read_hint(why: &Withheld<FsDeny>, layers: usize) -> String {
    match why {
        Withheld::MissingAllowance if layers > 1 => NESTED.to_string(),
        Withheld::MissingAllowance => {
            "running a program reads it: let the grant's fs `read` list name it".to_string()
        }
        Withheld::Excluded { .. } => EXCLUSION_HINT.to_string(),
    }
}

fn exec_denial(shown: &str, program: Refusing<'_>, why: &ExecDenial, error: Refusal) -> Denial {
    let deny = match why {
        ExecDenial::Run(Withheld::Excluded { scope, .. }) => Some(scope.to_string()),
        ExecDenial::Read(Withheld::Excluded { scope, .. }) => Some(scope.to_string()),
        _ => None,
    };
    let fields = [
        ("name", shown.to_string()),
        ("resolved", program.to_string()),
    ]
    .into_iter()
    .chain(deny.map(|deny| ("deny", deny)));
    Denial::new(Resource::Exec, fields, error)
}

/// Refuse a name the host would read as a DOS device (Windows only).  No grant
/// can admit it: it names no object to judge.
pub(crate) fn check_device_name(path: &LexicalPath) -> Result<(), Refusal> {
    path.reserved_device_refusal()
        .map_or(Ok(()), |m| Err(Refusal::new(m)))
}

/// Decide an `op` on the access a walk found: [`GrantStack::refusal_fs`] is
/// the decision, [`fs_denial`] the wording and the fact around it.
pub(crate) fn check_fs_exact(
    grants: &GrantStack,
    baseline: &Baseline,
    trace: &Trace,
    op: &FsOp,
) -> Result<(), Denial> {
    grants
        .refusal_fs(baseline, op, trace)
        .map_err(|refused| fs_denial(grants, refused, op, trace.real()))
}

/// The denial of `refused`, an `op` on the object at `real`.
pub(crate) fn fs_denial(
    grants: &GrantStack,
    refused: Refused,
    op: &FsOp,
    real: &RealPath,
) -> Denial {
    let name = <&str>::from(op);
    let path = shown(real.as_path());
    let layers = grants.fs().count();
    let (detail, error) = match refused {
        Refused::Withheld(Withheld::MissingAllowance) => {
            let permission = match op {
                FsOp::Read => "reading",
                FsOp::Write => "writing",
            };
            let error = Refusal::new(format!(
                "fs {name} denied by grant: no allowance for {permission} {path}"
            ));
            let hint = if layers > 1 {
                NESTED.to_string()
            } else {
                format!(
                    "add it to the grant's fs `{}` list",
                    match op {
                        FsOp::Read => "read` or `write",
                        FsOp::Write => "write",
                    }
                )
            };
            (None, error.with_hint(hint))
        }
        Refused::Withheld(Withheld::Excluded { scope, matched_by }) => (
            Some(("deny", scope.to_string())),
            Refusal::new(format!(
                "fs {name} denied by grant: {}",
                excluded(&path, &scope, matched_by)
            ))
            .with_hint(EXCLUSION_HINT),
        ),
        Refused::Guarded(pinned) => (
            Some(("pinned", pinned.to_string())),
            Refusal::new(format!(
                "fs write refused: {path} is the {pinned} binary ral pinned at startup, which \
                 confines every command run under a grant"
            ))
            .with_hint(
                "ral never writes into the binary that enforces its grants, whatever the \
                 grant allows. Install a new one by replacing the file rather than \
                 rewriting it: a rename leaves this session's pinned copy intact",
            ),
        ),
    };
    let fields = [("op", name.to_string()), ("path", path)]
        .into_iter()
        .chain(detail);
    Denial::new(Resource::Fs, fields, error)
}

/// Refuse a verb or builtin some layer of the stack withholds; `who` names it
/// in the refusal.  Silence permits.
pub(crate) fn check_flag(grants: &GrantStack, flag: Flag, who: &str) -> Result<(), Refusal> {
    if grants.permits(flag) {
        return Ok(());
    }
    Err(Refusal::new(match flag {
        Flag::Shell(ShellVerb::Detach) => format!(
            "{who}: an active grant withholds it, so nothing here may outlive this session. \
             Would `spawn` do, since it ends when the session does?"
        ),
        _ => format!("denied: {who} requires {}", flag.key()),
    }))
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
