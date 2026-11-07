//! `detach`: birth a process this session stops owning.  Ordinary
//! external-command machinery up to the double-fork birth, after which
//! nothing here can signal, await or reap the survivor — hence a `{pid, desc}`
//! receipt rather than a handle, and no log file anyone would have to find.
//!
//! A survivor born inside a `grant` keeps that confinement for life: the
//! projection is rendered into the launch as for a child we keep, minus the
//! parent-death tie ([`Ownership::Surrendered`](crate::authority::sandbox::Ownership)),
//! and nothing later can widen it because nothing later can name the process.
//!
//! Its standard descriptors are `/dev/null`: our end of an inherited pipe
//! closes when we exit, and the survivor's next write would take a `SIGPIPE`.

use crate::authority::policy::{Flag, ShellVerb};
use crate::fact::{AuditIo, Receipt};
use crate::ir::CommandName;
use crate::process::StdioSpec;
use crate::process::group::Survivor;
use crate::shell::runtime::{CommandOrigin, HandlerLookup, Mooring, Settled, Shell, Value, sig};

use super::head::Head;
use super::process::{Started, prepare, start_detached};

/// `detach <desc> <cmd> <args…>`, with `desc` already vetted by
/// [`crate::shell::builtins::concurrency`].  The head is an exec image by
/// definition: scope bindings are not consulted, and a handler in scope — a
/// user arm, the catch-all, or a base frame — runs inside this session, so it
/// is refused: nothing could be detached.  A stub stands in for `detach`
/// itself.
///
/// Two judgments follow resolution: that refusal, then [`Head::admit`] and
/// [`prepare`]'s vetting.
pub(crate) fn detach(
    desc: &str,
    head: &Value,
    argv: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let name = CommandName::of(&head.to_string());
    if let CommandName::Bare(bare) = &name {
        // Every pass of the stack: a catch-all intercepts as a per-name does.
        match shell.lookup_handler(bare) {
            Some(HandlerLookup::Frame(..)) => {
                return Err(sig(format!(
                    "detach: `{bare}` is handled here (by an arm for it, or by the catch-all) \
                     and a handler runs inside this session, so nothing can be detached. To \
                     stub `detach`, stand in for `detach` itself; to run the real program, \
                     `^{bare}`"
                )));
            }
            Some(HandlerLookup::Base(_)) => {
                return Err(sig(format!(
                    "detach: `{bare}` is ral's own, and runs inside this session; nothing \
                     can be detached"
                )));
            }
            None => {}
        }
    }
    let head = Head::resolve(&name, &shell.context);
    let shown = head.shown.clone();
    let start = shell.audit_start(mooring);
    let launched = (|| -> Settled<Started<Survivor>> {
        shell.check(Flag::Shell(ShellVerb::Detach), "detach")?;
        // The grant's verdict on the program, then existence (127/126) and argv
        // shape: the same judgments an ordinary external passes, so a bundled
        // uutils tool falls out as its own image with no special case here.  The
        // survivor outlives this run, but confining it happens here, under this
        // run's scope: a cancel during the stamp abandons the birth.
        let head = head.admit(mooring, shell)?;
        let mut prepared = prepare(
            head,
            argv,
            crate::authority::sandbox::Ownership::Surrendered,
            shell,
        )?;
        prepared
            .launch
            .stdin(StdioSpec::null())
            .stdout(StdioSpec::null())
            .stderr(StdioSpec::null());
        start_detached(prepared)
    })();
    let (shown, args, result) = match launched {
        Ok(Started {
            shown,
            args,
            born: Survivor::Named(pid),
        }) => (
            shown,
            args,
            Ok(Value::from_datum(Receipt {
                pid,
                desc: desc.into(),
            })),
        ),
        Ok(Started {
            shown,
            args,
            born: Survivor::Unnamed(why),
        }) => {
            let lost = sig(format!(
                "detach: '{shown}' was started and is running, but its pid was lost ({why}); it cannot be stopped by pid"
            ));
            (shown, args, Err(lost))
        }
        Err(broke) => (shown, Value::render_argv(argv), Err(broke)),
    };
    if let Some(start) = &start {
        let obs = shell.command_observation(
            start,
            &shown,
            args,
            CommandOrigin::Detached,
            result.as_ref().err(),
            AuditIo::default(),
        );
        shell.observe_stamped(Some(mooring), obs);
    }
    result
}
