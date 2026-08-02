//! `detach`: birth a process this session stops owning.  Ordinary
//! external-command machinery up to the double-fork birth, after which
//! nothing here can signal, await or reap the survivor — hence a `{pid, desc}`
//! receipt rather than a handle, and no log file anyone would have to find.
//!
//! A survivor born inside a `grant` keeps that confinement for life: the
//! projection is rendered into the launch as for a child we keep, minus the
//! parent-death tie ([`Ownership::Surrendered`](crate::sandbox::Ownership)),
//! and nothing later can widen it because nothing later can name the process.
//!
//! Its standard descriptors are `/dev/null`: our end of an inherited pipe
//! closes when we exit, and the survivor's next write would take a `SIGPIPE`.

use crate::ir::CommandName;
use crate::path::tilde::TildePath;
use crate::process::StdioSpec;
use crate::types::{HandlerLookup, Mooring, Settled, Shell, Value, sig};

use super::identity::CommandIdentity;
use super::io_event;
use super::process::build_command;
use super::vet::vet;

/// `detach <desc> <cmd> <args…>`, with `desc` already vetted by
/// [`crate::builtins::concurrency`].  The head is an exec image by
/// definition: scope bindings are not consulted, but a handler frame in
/// scope — a user frame or a base frame alike — runs and its value is the
/// `detach`'s; nothing is born, and nothing is spent from the budget.
///
/// Three judgments follow resolution: the frame's authority over the verb,
/// then [`vet`], then the session's remaining births.  The verb comes first
/// because a frame that withheld it is owed no opinion on which program was
/// named.
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
        // Every pass of the stack: a catch-all intercepts as a per-name
        // does, and a base frame runs rather than being spawned.
        match shell.lookup_handler(bare) {
            Some(HandlerLookup::Frame(entry, depth)) => {
                return crate::runtime::command_call::run_handler(
                    &entry, depth, argv, mooring, shell,
                );
            }
            Some(HandlerLookup::Base(entry)) => {
                let raw =
                    crate::runtime::command_call::run_base_frame(&entry, argv, &[], mooring, shell);
                return crate::evaluator::absorb_tail(raw, mooring, shell);
            }
            None => {}
        }
    }
    if !shell.permits_detach() {
        return Err(sig(
            "detach: an active grant withholds it, so nothing here may outlive this session. \
             Would `spawn` or `service` do, since both end when the session does?"
                .to_string(),
        ));
    }
    // Existence (127/126), argv shape, the grant's verdict on the whole call:
    // the same judgments an ordinary external passes, so a bundled uutils tool
    // falls out as its own image with no special case here.
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

    // The survivor outlives this run, but confining it happens here, under this
    // run's scope: a cancel during the stamp abandons the birth, which is the
    // one moment it still can.
    let mut launch = build_command(
        &plan,
        crate::sandbox::Ownership::Surrendered,
        shell,
        mooring.cancel.as_scope(),
    )?;
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
