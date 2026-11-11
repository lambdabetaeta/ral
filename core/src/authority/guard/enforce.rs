//! The in-process guards.
//!
//! Every runtime yes/no asked as an action is attempted, exec and its argv,
//! an fs read or write, a boolean flag, folds the whole dynamic
//! [`GrantStack`], so a verdict is authority intersected across every layer,
//! never one frame.  The judgments are the model's ([`GrantStack::admit`],
//! [`GrantStack::read`]); this module captures their refusals and, for the exec and fs
//! guards, which reach a real OS resource, builds the [`Denial`] the shell
//! records.  The sandbox projects the same authority off the same folds,
//! so the two cannot drift.

use crate::authority::policy::{
    Admitted, Baseline, ExecDenial, Flag, FsOp, GrantStack, LocateError, Program, Refused,
    ShellVerb, Withheld,
};
use crate::path::{LexicalPath, MatchedBy, Trace};
use std::fmt::{self, Display};

pub(crate) use crate::fact::Denial;
use crate::fact::{
    EditorFlag, ExclusionMatch, FlagId, ReadOrWrite, ShellFlag, SingleOrNested, Withholding,
    WithholdingExcluded,
};

fn layers(layers: impl Iterator) -> SingleOrNested {
    if layers.count() > 1 {
        SingleOrNested::Nested
    } else {
        SingleOrNested::Single
    }
}

impl From<MatchedBy> for ExclusionMatch {
    fn from(value: MatchedBy) -> Self {
        match value {
            MatchedBy::Name => Self::Name,
            MatchedBy::FoldedName => Self::FoldedName,
            MatchedBy::Identity => Self::Identity,
        }
    }
}

fn withholding<T, U>(
    why: Withheld<T>,
    nested: impl FnOnce() -> SingleOrNested,
    scope: impl FnOnce(T) -> U,
) -> Withholding<U> {
    match why {
        Withheld::MissingAllowance => Withholding::MissingAllowance(nested()),
        Withheld::Excluded {
            scope: denied,
            matched_by,
        } => Withholding::Excluded(WithholdingExcluded {
            scope: scope(*denied),
            matched_by: matched_by.into(),
        }),
    }
}

/// The domain carrier remains printable outside a computation.
fn excluded(subject: impl Display, scope: impl Display, matched_by: MatchedBy) -> String {
    match matched_by {
        MatchedBy::Name => format!("{subject} is excluded by {scope}"),
        MatchedBy::FoldedName => format!(
            "{subject} is excluded by {scope} after case or Unicode folding; \
             a deny holds under every spelling"
        ),
        MatchedBy::Identity => {
            format!(
                "{subject} is excluded by {scope} through a filesystem object reached under another name"
            )
        }
    }
}

/// What `refused` says of `subject`: the one wording of each fs refusal.
fn says(refused: &Refused, subject: impl Display) -> String {
    match refused {
        Refused::Guarded(pinned) => format!(
            "{subject} is the {} binary ral pinned at startup, which confines every \
             command run under a grant",
            <&str>::from(*pinned)
        ),
        Refused::Withheld(Withheld::MissingAllowance) => {
            format!("the grant does not allow {subject}")
        }
        Refused::Withheld(Withheld::Excluded { scope, matched_by }) => {
            excluded(subject, format!("{:?}", scope.to_string()), *matched_by)
        }
    }
}

/// What a refusal says of its object, which the caller names ahead of it.
impl Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&says(self, "it"))
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

/// Judge a program: the one exec door.
pub(crate) fn check_exec(
    grants: &GrantStack,
    baseline: &Baseline,
    shown: &str,
    program: Program,
) -> Result<Admitted, Box<Denial>> {
    let tool = match program {
        Program::Tool(name) => Some(name),
        Program::File { .. } => None,
    };
    grants
        .admit(baseline, program)
        .map_err(|why| Box::new(exec_denial(grants, baseline, shown, tool, why)))
}

/// Judge a name a head's walk reached no object at, as running it would be.
pub(crate) fn check_exec_name(
    grants: &GrantStack,
    baseline: &Baseline,
    shown: &str,
    trace: &Trace,
) -> Result<(), Box<Denial>> {
    grants
        .admit_name(baseline, trace)
        .map_err(|why| Box::new(exec_denial(grants, baseline, shown, None, why)))
}

fn exec_denial(
    grants: &GrantStack,
    baseline: &Baseline,
    shown: &str,
    tool: Option<&str>,
    why: ExecDenial,
) -> Denial {
    match why {
        ExecDenial::Run(why) => Denial::exec_run(
            shown.into(),
            withholding(why, || layers(grants.exec()), |scope| scope.target()),
        ),
        ExecDenial::Read(why) => {
            let cause = withholding(why, || layers(grants.fs()), |scope| scope.to_string());
            match (tool, cause) {
                (Some(tool), Withholding::Excluded(exclusion)) => Denial::bundled_read(
                    shown.into(),
                    tool.into(),
                    baseline.image().map(|image| image.real().to_string()),
                    exclusion,
                ),
                (_, cause) => Denial::exec_read(shown.into(), cause),
            }
        }
    }
}

/// Refuse a name the host would read as a DOS device (Windows only).  No grant
/// can admit it: it names no object to judge.
pub(crate) fn check_device_name(path: &LexicalPath) -> Result<(), Denial> {
    path.reserved_device_refusal()
        .map_or(Ok(()), |device| Err(Denial::device(path.shown(), device)))
}

/// Name a refusal by the request, never by a withheld link's target.
pub(crate) fn fs_denial(
    grants: &GrantStack,
    refused: Refused,
    op: FsOp,
    path: &LexicalPath,
) -> Denial {
    match refused {
        Refused::Withheld(why) => Denial::fs_withheld(
            path.shown(),
            match op {
                FsOp::Read => ReadOrWrite::Read,
                FsOp::Write => ReadOrWrite::Write,
            },
            withholding(why, || layers(grants.fs()), |scope| scope.to_string()),
        ),
        Refused::Guarded(pinned) => Denial::pinned_write(path.shown(), pinned),
    }
}

/// Flags do not introduce an audit observation.
pub(crate) fn check_flag(grants: &GrantStack, flag: Flag, who: &str) -> Result<(), Denial> {
    if grants.permits(flag) {
        return Ok(());
    }
    let flag = match flag {
        Flag::Editor(verb) => FlagId::Editor(match verb {
            crate::authority::policy::EditorVerb::Read => EditorFlag::Read,
            crate::authority::policy::EditorVerb::Write => EditorFlag::Write,
            crate::authority::policy::EditorVerb::Tui => EditorFlag::Tui,
        }),
        Flag::Shell(verb) => FlagId::Shell(match verb {
            ShellVerb::Chdir => ShellFlag::Chdir,
            ShellVerb::Detach => ShellFlag::Detach,
        }),
    };
    Err(Denial::flag(who.into(), flag))
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
