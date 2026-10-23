//! The in-process guards.
//!
//! Every runtime yes/no asked as an action is attempted, exec and its argv,
//! an fs read or write, a boolean flag, folds the whole dynamic
//! [`GrantStack`], so a verdict is authority intersected across every layer,
//! never one frame.  The judgments are the model's ([`GrantStack::admit`],
//! [`GrantStack::read`]); this module words their refusals and, for the exec and fs
//! guards, which reach a real OS resource, builds the [`Denial`] the Shell's
//! records.  The sandbox projects the same authority off the same folds,
//! so the two cannot drift.

use crate::capability::{
    Admitted, ExecDenial, ExecRefused, Flag, Floor, FsDeny, FsOp, GrantStack, LocateError, Program,
    Refused, ShellVerb, Withheld, ral_binary,
};
use crate::path::shown;
use crate::path::{LexicalPath, MatchedBy, RealPath, Trace};
use crate::text::code;
use crate::types::{Check, Context, Error, Resource, Settled, sig};
use std::fmt::{self, Display};

/// A refused exec or fs check: the fact the trail keeps, and the error the
/// caller raises.  An admitted check is never recorded: a trail of every
/// permitted read says nothing an auditor asked.
pub(crate) struct Denial {
    pub check: Check,
    pub error: Error,
}

impl Denial {
    fn new(
        resource: Resource,
        fields: impl IntoIterator<Item = (&'static str, String)>,
        error: Error,
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

/// Judge a program and its argv against the stack's exec rules.
pub(crate) fn check_exec(
    ctx: &Context,
    floor: &Floor,
    shown: &str,
    program: Program,
    args: Vec<String>,
) -> Result<Admitted, Box<Denial>> {
    ctx.grants.admit(floor, program, args).map_err(|refused| {
        let ExecRefused { program, args, why } = *refused;
        let error = exec_error(&ctx.grants, shown, &program, &why);
        Box::new(exec_denial(shown, &program, &why, Some(&args), error))
    })
}

/// The words of an exec refusal.
fn exec_error(grants: &GrantStack, shown: &str, program: &Program, why: &ExecDenial) -> Error {
    let layers = grants.exec().count();
    let head = format!("command '{shown}' denied by active grant");
    match why {
        ExecDenial::Run(Withheld::MissingAllowance) if layers > 1 => Error::new(format!(
            "{head}: some layer's exec `allow` list does not let it run"
        ))
        .with_hint(NESTED),
        ExecDenial::Run(Withheld::MissingAllowance) => Error::new(format!(
            "{head}: nothing in the grant's exec `allow` list lets it run"
        ))
        .with_hint(format!(
            "add it to the grant's exec `allow` list: {} for a command name, {} for a file, \
                 or {} for its directory",
            code("`name git"),
            code("`absolute /path/to/it"),
            code("`dir `absolute /path"),
        )),
        ExecDenial::Run(Withheld::Excluded { scope, matched_by }) => Error::new(format!(
            "{head}: {}",
            excluded(
                format_args!("'{program}'"),
                code(&scope.to_string()),
                *matched_by
            )
        ))
        .with_hint(EXCLUSION_HINT),
        ExecDenial::Read(why) => Error::new(format!("{head}: {}", read_cause(shown, program, why)))
            .with_hint(read_hint(why, grants.fs().count())),
    }
}

/// Why running `program` is refused for what it reads: a host file, or for a
/// bundled tool ral's binary, which it runs.
fn read_cause(shown: &str, program: &Program, why: &Withheld<FsDeny>) -> String {
    let (lead, subject) = match program {
        Program::File { located, .. } => (String::new(), format!("'{}'", located.real())),
        Program::Tool(_) => (
            format!("'{shown}' is built into ral, and "),
            ral_binary().map_or_else(
                || "ral's binary".to_string(),
                |own| format!("ral's binary '{}'", own.real()),
            ),
        ),
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

fn exec_denial(
    shown: &str,
    program: &Program,
    why: &ExecDenial,
    args: Option<&[String]>,
    error: Error,
) -> Denial {
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
    .chain(args.map(|args| ("args", args.join(" "))))
    .chain(deny.map(|deny| ("deny", deny)));
    Denial::new(Resource::Exec, fields, error)
}

/// The refusal of a head whose program the stack denies, raised before any
/// argv is known: the same trail as [`check_exec`]'s, without the `args`.
pub(crate) fn deny_head(
    grants: &GrantStack,
    shown: &str,
    program: &Program,
    why: &ExecDenial,
) -> Denial {
    let error = exec_error(grants, shown, program, why);
    exec_denial(shown, program, why, None, error)
}

/// Refuse a name the host would read as a DOS device (Windows only).  No grant
/// can admit it: it names no object to judge.
pub(crate) fn check_device_name(path: &LexicalPath) -> Settled<()> {
    path.reserved_device_refusal()
        .map_or(Ok(()), |m| Err(sig(m)))
}

/// Decide an `op` on the access a walk found: [`GrantStack::refusal_fs`] is
/// the decision, [`fs_denial`] the wording and the fact around it.
pub(crate) fn check_fs_exact(
    ctx: &Context,
    floor: &Floor,
    trace: &Trace,
    op: &FsOp,
) -> Result<(), Box<Denial>> {
    (ctx.grants.refusal_fs(floor, op, trace))
        .map_err(|refused| fs_denial(ctx, refused, op, trace.real()))
}

/// The denial of `refused`, an `op` on the object at `real`.
pub(crate) fn fs_denial(
    ctx: &Context,
    refused: Refused,
    op: &FsOp,
    real: &RealPath,
) -> Box<Denial> {
    let name = <&str>::from(op);
    let path = shown(real.as_path());
    let layers = ctx.grants.fs().count();
    let (detail, error) = match refused {
        Refused::Withheld(Withheld::MissingAllowance) => {
            let permission = match op {
                FsOp::Read => "reading",
                FsOp::Write => "writing",
            };
            let error = Error::new(format!(
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
            Error::new(format!(
                "fs {name} denied by grant: {}",
                excluded(&path, &scope, matched_by)
            ))
            .with_hint(EXCLUSION_HINT),
        ),
        Refused::Guarded(pinned) => (
            Some(("pinned", pinned.to_string())),
            Error::new(format!(
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
    Box::new(Denial::new(Resource::Fs, fields, error))
}

/// Refuse a verb or builtin some layer of the stack withholds; `who` names it
/// in the refusal.  Silence permits.
pub(crate) fn check_flag(ctx: &Context, flag: Flag, who: &str) -> Settled<()> {
    if ctx.grants.permits(flag) {
        return Ok(());
    }
    Err(sig(match flag {
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
