//! The shell's `detach` operation: admit a surrendered command, then report
//! its receipt and audit observation without claiming the surviving child.

use super::Command;
use crate::authority::policy::{Flag, ShellVerb};
use crate::error::Error;
use crate::fact::{AuditIo, Receipt};
use crate::ir::CommandName;
use crate::process::group::Survivor;
use crate::shell::runtime::{CommandOrigin, HandlerLookup, Mooring, Settled, Shell, Value};

/// `detach <desc> <cmd> <args…>`: the double-forked survivor is nobody's to
/// signal, await or reap, so the result is a `{pid, desc}` receipt. A command
/// answered by a handler, which would run inside this session, is refused.
pub(in crate::shell) fn detach(
    desc: &str,
    command: &Value,
    argv: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let name = CommandName::of(&command.to_string());
    if let CommandName::Bare(bare) = &name {
        // Every pass of the stack: a catch-all intercepts as a per-name does.
        match shell.lookup_handler(bare) {
            Some(HandlerLookup::Frame(..)) => {
                return Err(Error::new(format!(
                    "detach: `{bare}` is handled here (by an arm for it, or by the catch-all) \
                     and a handler runs inside this session, so nothing can be detached. To \
                     stub `detach`, stand in for `detach` itself; to run the real program, \
                     `^{bare}`"
                ))
                .into());
            }
            Some(HandlerLookup::Base(_)) => {
                return Err(Error::new(format!(
                    "detach: `{bare}` is ral's own, and runs inside this session; nothing \
                     can be detached"
                ))
                .into());
            }
            None => {}
        }
    }
    let command = Command::from_name(&name, &shell.context);
    let shown = command.shown.clone();
    let start = shell.audit_start(mooring);
    let launched = (|| -> Settled<Survivor> {
        shell.check(Flag::Shell(ShellVerb::Detach), "detach")?;
        // The grant's verdict on the program, then existence (127/126) and argv
        // shape: the same judgments an ordinary external passes, so a bundled
        // uutils tool falls out as its own image with no special case here.  The
        // survivor outlives this run, but its start is under this run's scope:
        // a stop before the fork makes no survivor; one while its trampoline
        // is silent leaves it.
        command.admit(mooring, shell)?.detach(argv, shell)
    })();
    let result = launched.and_then(|survivor| match survivor {
        Survivor::Named(pid) => Ok(Value::from_datum(Receipt {
            pid,
            desc: desc.into(),
        })),
        Survivor::Unnamed(why) => {
            let lost = Error::new(format!(
                "detach: '{shown}' was started and is running, but its pid was lost ({why}); it cannot be stopped by pid"
            ));
            Err(lost.into())
        }
    });
    if let Some(start) = &start {
        let obs = shell.command_observation(
            start,
            &shown,
            Value::render_argv(argv),
            CommandOrigin::Detached,
            result.as_ref().err(),
            AuditIo::default(),
        );
        shell.observe_stamped(Some(mooring), obs);
    }
    result
}
