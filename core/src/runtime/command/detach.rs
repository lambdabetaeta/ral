//! The birth of a process this session stops owning.
//!
//! Everything here is the ordinary external-command machinery —
//! [`CommandIdentity`], [`vet`](super::vet), [`build_command`] — up to the
//! single act that differs: the child is born by double-fork
//! ([`Launch::spawn_detached`](crate::process::Launch::spawn_detached)), so
//! its pgid is never observed by this process and nothing here can signal,
//! await, or reap it.
//!
//! What replaces the handle is a **receipt**: `{pid, desc}`, first-order data
//! the caller keeps.  Nothing is written anywhere: a program worth outliving
//! a session configures its own logging, and files invented here would be
//! litter nobody reads (`decisions/260725_survives-exit-is-its-own-verb`).
//!
//! A survivor born inside a `grant` keeps that grant's confinement for
//! life.  The projection is rendered into the launch exactly as it is for
//! a child we keep — only the parent-death tie is dropped
//! ([`Ownership::Surrendered`](crate::sandbox::Ownership)) — so what the
//! process may read, write and reach is frozen as the birthing frame left
//! it.  Nothing can widen it afterwards, since no later frame, and no
//! later session, can name the process at all.  The frame's authority over
//! the verb is a separate question, asked below and answered by
//! `detach:` on the capability stack.
//!
//! All three of the survivor's standard descriptors are therefore
//! `/dev/null`.  What rules out *inheriting* them is the pipe, not the bytes:
//! this process's end closes when it exits, and the survivor's next write
//! would take a `SIGPIPE`.  `/dev/null` answers that as a file would.

use crate::ir::CommandName;
use crate::path::tilde::TildePath;
use crate::process::StdioSpec;
use crate::types::{Mooring, Settled, Shell, Value, sig};

use super::identity::CommandIdentity;
use super::io_event;
use super::process::build_command;
use super::vet::vet;

/// `detach <desc> <cmd> <args…>`, past the surface discipline
/// [`crate::builtins::concurrency`] applies to `desc`.
///
/// The head is an exec image by definition, so scope bindings are not
/// consulted, and a builtin name is refused — there is no process image to
/// leave running.  A handled name is *not* refused: `detach` is not the one
/// place in ral where a handler in scope errors instead of dispatching
/// ([`crate::runtime::command_call`]), so the handler runs and its value is
/// the value of the `detach`.  Nothing is born on that route, and so nothing
/// is spent from the budget.
///
/// Three judgments follow resolution, in this order: the frame's authority
/// over the verb ([`Shell::permits_detach`]), the grant's verdict on the
/// command itself ([`vet`]), and the session's remaining births.  The
/// verb's own authority comes first because a frame that withheld it is
/// not owed an opinion on which program was named.
pub(crate) fn detach(
    desc: &str,
    head: &Value,
    argv: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let spelled = head.to_string();
    let name = if let Some(tilde) = TildePath::parse(&spelled) {
        CommandName::TildePath(tilde)
    } else if spelled.contains('/') {
        CommandName::Path(spelled)
    } else {
        CommandName::Bare(spelled)
    };
    if let CommandName::Bare(bare) = &name {
        if shell.lookup_builtin(bare).is_some() {
            return Err(sig(format!(
                "detach: '{bare}' is a ral builtin, so there is no process image to detach. \
                 Which program did you mean to leave running?"
            )));
        }
        // `lookup_handler` answers for both passes of the handler stack, so a
        // catch-all frame intercepts here exactly as a per-name one does.
        if let Some((entry, depth)) = shell.lookup_handler(bare) {
            return crate::runtime::command_call::run_handler(&entry, depth, argv, mooring, shell);
        }
    }
    if !shell.permits_detach() {
        return Err(sig(
            "detach: an active grant withholds it, so nothing here may outlive this session. \
             Would `spawn` or `service` do, since both end when the session does?"
                .to_string(),
        ));
    }
    // Nothing is bypassed: existence (127/126), argv shape, and the grant's
    // verdict on the whole call are the same three judgments an ordinary
    // external passes, so a bundled uutils tool falls out as its own image
    // with no special case here.
    let plan = vet(
        &CommandIdentity::resolve(name, &shell.mobile.context),
        argv,
        shell,
    )?;

    let Some(policy) = shell.detach_policy() else {
        return Err(sig(
            "detach: this host installed the builtin but armed no detach policy; \
             installing and arming are one act, so this is an internal inconsistency."
                .to_string(),
        ));
    };
    if let Err(budget) = policy.admit() {
        return Err(sig(format!(
            "detach: this session has already birthed {budget} processes it no longer owns; \
             that is the budget, and nothing gives a birth back. Is one of them finished, so \
             you can stop it by pid and do without another?"
        )));
    }

    let mut launch = build_command(&plan, crate::sandbox::Ownership::Surrendered, shell)?;
    launch.stdin(StdioSpec::null());
    launch.stdout(StdioSpec::null());
    launch.stderr(StdioSpec::null());
    let pid = launch
        .spawn_detached()
        .map_err(|e| sig(format!("detach: cannot launch '{}': {e}", plan.shown)))?;

    mooring.emit_io(&io_event::exec(&plan.shown, &plan.args, 0));
    shell.mobile.control.last_status = 0;
    Ok(Value::map(vec![
        ("pid".into(), Value::Int(i64::from(pid))),
        ("desc".into(), Value::String(desc.to_string())),
    ]))
}
