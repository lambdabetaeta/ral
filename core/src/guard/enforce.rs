//! The in-process guards.
//!
//! Every runtime yes/no asked as an action is attempted, exec and its argv,
//! an fs read or write, a boolean flag, folds the whole dynamic
//! [`GrantStack`], so a verdict is authority intersected across every layer,
//! never one frame.  The judgments are the model's ([`GrantStack::admit`],
//! [`GrantStack::read`]); this module words their refusals and, for the exec and fs
//! guards, which reach a real OS resource, builds the [`Denial`] the Shell's
//! door records.  The sandbox projects the same authority off the same folds,
//! so the two cannot drift.

use crate::capability::{
    Admitted, ExecDenial, Flag, FsDeny, FsOp, GrantStack, Program, Refused, ShellVerb, Withheld,
    ral_binary,
};
use crate::path::shown;
use crate::path::{LexicalPath, MatchedBy};
use crate::text::code;
use crate::types::{Check, Context, Error, Resource, Settled, sig};
use std::fmt::Display;

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

/// The exclusion half of a refusal, which no allowance overrides.
fn excluded(subject: impl Display, scope: impl Display, matched_by: MatchedBy) -> String {
    match matched_by {
        MatchedBy::Name => {
            format!("{subject} is excluded by {scope}; an allowance cannot override an exclusion")
        }
        MatchedBy::FoldedName => format!(
            "{subject} is the denied {scope} under another spelling \
             (case or Unicode form); a deny holds under every spelling"
        ),
        MatchedBy::Identity => format!("{subject} is the denied object {scope} under another name"),
    }
}

const EXCLUSION_HINT: &str =
    "the exclusion belongs to the policy that imposes it; change that policy";

/// Judge a program and its argv against the stack's exec rules.
pub(crate) fn check_exec(
    ctx: &Context,
    shown: &str,
    program: Program,
    args: Vec<String>,
) -> Result<Admitted, Box<Denial>> {
    ctx.grants.admit(program, args).map_err(|refused| {
        let Refused { program, args, why } = *refused;
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
        Program::File { real, .. } => (String::new(), format!("'{real}'")),
        Program::Tool(_) => (
            format!("'{shown}' is built into ral, and "),
            ral_binary().map_or_else(
                || "ral's binary".to_string(),
                |own| format!("ral's binary '{own}'"),
            ),
        ),
    };
    match (why, program) {
        (Withheld::MissingAllowance, _) => {
            format!("{lead}{subject} may run, but it is not readable under the active grant")
        }
        (Withheld::Excluded { scope, matched_by }, _) => format!(
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

/// Why the fs guard refuses: `Guarded` is the one refusal no grant decides, a
/// write onto a binary the sandbox pinned at boot, made before the stack is
/// consulted, the twin of the discard device, which is admitted before it is.
pub(super) enum FsRefusal {
    Guarded(&'static str),
    Withheld(Withheld<FsDeny>),
}

/// Judge the resolved path by `op`'s region: an exclusion beats every
/// allowance, and a path no allowance supports is withheld.  `Ok` exactly when
/// no layer held an `fs` opinion or some allowance supports the path alone.
///
/// The fold re-resolves exclusions against the live disk on every check, so the exclusions
/// this decides against are the ones the disk describes now, where the
/// sandbox's projection folds once, at spawn, because that is when the OS
/// profile is written.
pub(super) fn fs_judge(
    grants: &GrantStack,
    resolved: &std::path::Path,
    op: &FsOp,
) -> Result<(), FsRefusal> {
    // Before the stack: an empty stack is exactly the case it must catch.
    if matches!(op, FsOp::Write)
        && let Some(pinned) = crate::sandbox::pinned_binary(resolved)
    {
        return Err(FsRefusal::Guarded(pinned));
    }
    match op {
        FsOp::Read => grants.read(resolved),
        FsOp::Write => grants.write(resolved),
    }
    .map_err(FsRefusal::Withheld)
}

impl GrantStack {
    /// The fs guard's verdict as a plain bool, for callers with no [`Context`]
    /// to audit through: exarch's boot-time skill discovery asks it of a
    /// one-frame [`GrantStack::of`].  The same [`fs_judge`] the guard runs,
    /// canonicalising leniently inside as [`Shell::check_fs_read`] does, so
    /// there is no surface-form spelling of the question.  The
    /// [`LexicalPath::is_discard`] exemption is the Shell door's alone: it
    /// excuses a discard device from an *access*, and this door decides
    /// membership, not access.
    ///
    /// [`Shell::check_fs_read`]: crate::types::Shell::check_fs_read
    pub fn admits_fs(&self, op: &FsOp, path: &LexicalPath) -> bool {
        self.admits_fs_exact(op, &path.canonicalise_lenient())
    }

    /// [`admits_fs`](Self::admits_fs) for a path already symlink-free, what
    /// [`crate::path::Located::real`] hands over, so no second walk gets to
    /// disagree with the one that located the object.
    pub fn admits_fs_exact(&self, op: &FsOp, real: &std::path::Path) -> bool {
        fs_judge(self, real, op).is_ok()
    }
}

/// Refuse a name the host would read as a DOS device (Windows only).  No grant
/// can admit it: it names no object to judge.
pub(crate) fn check_device_name(path: &LexicalPath) -> Settled<()> {
    path.reserved_device_refusal()
        .map_or(Ok(()), |m| Err(sig(m)))
}

/// Decide an `op` on a symlink-free path: [`fs_judge`] is the decision,
/// this the wording and the fact around it.
pub(crate) fn check_fs_exact(
    ctx: &Context,
    resolved: &std::path::Path,
    op: &FsOp,
) -> Result<(), Box<Denial>> {
    let name = <&str>::from(op);
    let path = shown(resolved);
    let layers = ctx.grants.fs().count();
    let refusal = match fs_judge(&ctx.grants, resolved, op) {
        Ok(()) => return Ok(()),
        Err(refusal) => refusal,
    };
    let (detail, error) = match refusal {
        FsRefusal::Withheld(Withheld::MissingAllowance) => {
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
        FsRefusal::Withheld(Withheld::Excluded { scope, matched_by }) => (
            Some(("deny", scope.to_string())),
            Error::new(format!(
                "fs {name} denied by grant: {}",
                excluded(&path, &scope, matched_by)
            ))
            .with_hint(EXCLUSION_HINT),
        ),
        FsRefusal::Guarded(pinned) => (
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
    Err(Box::new(Denial::new(Resource::Fs, fields, error)))
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
mod tests;
