//! The in-process guards.
//!
//! Every runtime yes/no asked as an action is attempted, exec and its argv,
//! an fs read or write, a boolean flag, folds the whole dynamic
//! [`GrantStack`], so a verdict is authority intersected across every layer,
//! never one frame.  The judgments are the model's ([`GrantStack::admit`],
//! [`region`]); this module words their refusals and, for the exec and fs
//! guards, which reach a real OS resource, builds the [`Denial`] the Shell's
//! door records.  The sandbox projects the same authority off the same folds,
//! so the two cannot drift.

use crate::capability::{
    Admitted, ExecDenial, Flag, FsOp, GrantStack, Program, Refused, Verdict, region,
};
use crate::path::{FrozenPath, LexicalPath, Resolver};
use crate::types::{Check, Context, Error, Resource, Settled, sig};

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

/// Judge a program and its argv against the stack's exec rules.
pub(crate) fn check_exec(
    ctx: &Context,
    shown: &str,
    program: Program,
    args: Vec<String>,
) -> Result<Admitted, Box<Denial>> {
    ctx.grants
        .admit(program, args)
        .map_err(|refused| Box::new(exec_denial(shown, refused)))
}

fn exec_denial(shown: &str, Refused { program, args, why }: Refused) -> Denial {
    let respelled = match &why {
        ExecDenial::Denied { respelled } => respelled.as_ref(),
        ExecDenial::Subcommand { .. } => None,
    };
    let error = match &why {
        ExecDenial::Denied {
            respelled: Some(deny),
        } => Error::new(respelled_refusal("exec", &program, deny)),
        ExecDenial::Denied { respelled: None } => {
            Error::new(format!("command '{shown}' denied by active grant")).with_hint(
                "add the command to the grant exec map \
                 (or its directory, keyed with a trailing '/') to allow it",
            )
        }
        ExecDenial::Subcommand { allowed } => {
            let hint = format!(
                "allowed subcommands (matched against the command's first argument): {}",
                allowed.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            match args.first() {
                Some(first) => Error::new(format!(
                    "command '{shown}' subcommand '{first}' denied by active grant"
                )),
                None => Error::new(format!(
                    "command '{shown}' requires an allowed subcommand under the active grant"
                )),
            }
            .with_hint(hint)
        }
    };
    let fields = [
        ("name", shown.to_string()),
        ("resolved", program.to_string()),
        ("args", args.join(" ")),
    ]
    .into_iter()
    .chain(respelled.map(|deny| ("deny", deny.to_string())));
    Denial::new(Resource::Exec, fields, error)
}

/// The refusal of a head whose program the stack denies, raised before any
/// argv is known; the check names only the head as written.
pub(crate) fn deny_head(grants: &GrantStack, shown: &str, program: &Program) -> Denial {
    let error = match grants.respelled(program) {
        Some(deny) => Error::new(respelled_refusal("exec", program, deny)),
        None => Error::new(format!(
            "command '{shown}' denied by active grant ({program})"
        ))
        .with_hint("add the command to the grant exec map to allow it"),
    };
    Denial::new(Resource::Exec, [("name", shown.to_string())], error)
}

/// The decision half of [`check_fs_exact`]: does the stack admit `op` on the
/// resolved path?  `Unrestricted` exactly when no layer held an `fs`
/// opinion.  `Guarded` is the one verdict no grant decides: a write onto a
/// binary the sandbox pinned at boot, refused before the stack is consulted
/// — the twin of the discard device, which is admitted before it is.
/// `Respelled` is a denial by a deny that holds the path only under another
/// spelling of its name, so the refusal can say why.
pub(super) enum FsVerdict {
    Unrestricted,
    Granted,
    Denied,
    Respelled(FrozenPath),
    Guarded(&'static str),
}

/// Judge the resolved path by `op`'s region: a deny outranks every allow, and
/// a path no prefix holds is denied.
///
/// The fold runs against a live [`Resolver`] on every check, so the regions
/// this decides against are the ones the disk describes now, where the
/// sandbox's projection folds once, at spawn, because that is when the OS
/// profile is written.  That freshness is the whole difference between the
/// two consumers of the fold.
pub(super) fn fs_verdict(
    grants: &GrantStack,
    resolver: &Resolver,
    resolved: &std::path::Path,
    op: &FsOp,
) -> FsVerdict {
    // Before the stack: an empty stack is exactly the case it must catch.
    if matches!(op, FsOp::Write)
        && let Some(pinned) = crate::sandbox::pinned_binary(resolved)
    {
        return FsVerdict::Guarded(pinned);
    }
    let Some(region) = region(grants, resolver, op) else {
        return FsVerdict::Unrestricted;
    };
    match region.verdict(resolved) {
        Verdict::Deny => region
            .respelled(resolved)
            .map_or(FsVerdict::Denied, |deny| FsVerdict::Respelled(deny.clone())),
        _ => FsVerdict::Granted,
    }
}

impl GrantStack {
    /// The fs guard's verdict as a plain bool, for callers with no [`Context`]
    /// to audit through: exarch's boot-time skill discovery asks it of a
    /// one-frame [`GrantStack::of`].  The same [`fs_verdict`] the guard runs,
    /// canonicalising leniently inside as [`Shell::check_fs_read`] does, so
    /// there is no surface-form spelling of the question.  The
    /// [`LexicalPath::is_discard`] exemption is the Shell door's alone: it
    /// excuses a discard device from an *access*, and this door decides
    /// membership, not access.
    ///
    /// [`Shell::check_fs_read`]: crate::types::Shell::check_fs_read
    pub fn admits_fs(&self, op: &FsOp, resolver: &Resolver, path: &LexicalPath) -> bool {
        self.admits_fs_exact(op, resolver, &path.canonicalise_lenient())
    }

    /// [`admits_fs`](Self::admits_fs) for a path already symlink-free, what
    /// [`crate::path::Located::real`] hands over, so no second walk gets to
    /// disagree with the one that located the object.
    pub fn admits_fs_exact(&self, op: &FsOp, resolver: &Resolver, real: &std::path::Path) -> bool {
        matches!(
            fs_verdict(self, resolver, real, op),
            FsVerdict::Unrestricted | FsVerdict::Granted
        )
    }
}

/// Refuse a name the host would read as a DOS device (Windows only).  No grant
/// can admit it: it names no object to judge.
pub(crate) fn check_device_name(path: &LexicalPath) -> Settled<()> {
    path.reserved_device_refusal()
        .map_or(Ok(()), |m| Err(sig(m)))
}

/// Decide an `op` on a symlink-free path: [`fs_verdict`] is the decision,
/// this the wording and the fact around it.
pub(crate) fn check_fs_exact(
    ctx: &Context,
    resolved: &std::path::Path,
    op: &FsOp,
) -> Result<(), Box<Denial>> {
    let name = <&str>::from(op);
    let (detail, error) = match fs_verdict(&ctx.grants, &ctx.resolver(), resolved, op) {
        FsVerdict::Unrestricted | FsVerdict::Granted => return Ok(()),
        FsVerdict::Denied => (
            None,
            Error::new(format!("fs {name} denied by grant: {}", resolved.display())),
        ),
        FsVerdict::Respelled(deny) => (
            Some(("deny", deny.as_str().to_string())),
            Error::new(respelled_refusal(
                format_args!("fs {name}"),
                resolved.display(),
                deny.as_str(),
            )),
        ),
        FsVerdict::Guarded(pinned) => (
            Some(("pinned", pinned.to_string())),
            Error::new(format!(
                "fs write refused: {} is the {pinned} binary ral pinned at startup, which \
                 confines every command run under a grant",
                resolved.display()
            ))
            .with_hint(
                "ral never writes into the binary that enforces its grants, whatever the \
                 grant allows. Install a new one by replacing the file rather than \
                 rewriting it: a rename leaves this session's pinned copy intact",
            ),
        ),
    };
    let fields = [
        ("op", name.to_string()),
        ("path", resolved.display().to_string()),
    ]
    .into_iter()
    .chain(detail);
    Err(Box::new(Denial::new(Resource::Fs, fields, error)))
}

/// The refusal of `op` on `path` by a deny that holds it only under another
/// spelling of its name; fs and exec say it alike.
fn respelled_refusal(
    op: impl std::fmt::Display,
    path: impl std::fmt::Display,
    deny: impl std::fmt::Display,
) -> String {
    format!(
        "{op} denied by grant: {path} is the denied {deny} under another spelling \
         (case or Unicode form); a deny holds under every spelling"
    )
}

/// Refuse a verb or builtin some layer of the stack withholds; `who` names it
/// in the refusal.  Silence permits.
pub(crate) fn check_flag(ctx: &Context, flag: Flag, who: &str) -> Settled<()> {
    if ctx.grants.permits(flag) {
        return Ok(());
    }
    Err(sig(match flag {
        Flag::Detach => format!(
            "{who}: an active grant withholds it, so nothing here may outlive this session. \
             Would `spawn` or `service` do, since both end when the session does?"
        ),
        _ => format!("denied: {who} requires {}", flag.key()),
    }))
}

#[cfg(all(test, unix))]
mod tests;
