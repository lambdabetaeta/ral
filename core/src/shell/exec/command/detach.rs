//! `detach`: start a program the session will not own.

use super::{Command, Prepared};
use crate::authority::policy::{Flag, ShellVerb};
use crate::authority::sandbox::Ownership;
use crate::error::Error;
use crate::fact::Receipt;
use crate::ir::CommandName;
use crate::process::{Scope, StdioSpec, group::Survivor};
use crate::shell::runtime::{CommandOrigin, Mooring, Settled, Shell, ValClos};

/// `detach <desc> <program> <args>`: resolve `program` as `^program` would,
/// and return the receipt `[pid: Int, desc: String]`. The session never waits for,
/// signals or reaps it.
pub(in crate::shell) fn detach(
    desc: &str,
    program: &str,
    argv: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    shell.check(Flag::Shell(ShellVerb::Detach), "detach")?;
    let command = Command::from_name(&CommandName::of(program), &shell.context)
        .lookup(&shell.context.grants, shell.baseline())
        .admit(mooring, shell)?;
    let origin = CommandOrigin::Detached;
    command.with_record(argv, origin, None, mooring, shell, |command, shell| {
        let pid = command
            .prepare(argv, Ownership::Surrendered, shell)?
            .detach(&shell.scope)?;
        Ok(ValClos::from_datum(Receipt {
            pid,
            desc: desc.into(),
        }))
    })
}

impl Prepared {
    /// Spawn detached, with null streams, and return the pid. Private to
    /// this file, so only the builtin above, after its permission check,
    /// can detach anything.
    fn detach(self, scope: &Scope) -> Settled<u32> {
        let Self {
            mut launch,
            confinement,
            program,
        } = self;
        launch
            .launch_mut()
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Null)
            .stderr(StdioSpec::Null);
        let survivor = launch
            .spawn_detached(scope)
            .map_err(|why| program.unstarted(why, &confinement))?;
        match survivor {
            Survivor::Named(pid) => Ok(pid),
            Survivor::Unnamed(cause) => Err(Error::new(crate::fact::Reason::Launch(Box::new(
                crate::fact::LaunchFailure::DetachedPid(crate::fact::DetachedPidFailure {
                    program: program.shown,
                    cause,
                }),
            )))
            .into()),
        }
    }
}
