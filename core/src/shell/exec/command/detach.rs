//! `detach`: start a program the session will not own.

use super::{Command, Prepared};
use crate::authority::policy::{Flag, ShellVerb};
use crate::authority::sandbox::Ownership;
use crate::error::Error;
use crate::fact::Receipt;
use crate::ir::CommandName;
use crate::process::{Scope, StdioSpec, group::Survivor};
use crate::shell::runtime::{CommandOrigin, Mooring, Settled, Shell, Value};

/// `detach <desc> <cmd> <args…>`: start `cmd` as a program, as `^cmd` would,
/// and return the receipt `{pid, desc}`. The session never waits for,
/// signals or reaps it.
pub(in crate::shell) fn detach(
    desc: &str,
    command: &Value,
    argv: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    shell.check(Flag::Shell(ShellVerb::Detach), "detach")?;
    let command = Command::from_name(&CommandName::of(&command.to_string()), &shell.context)
        .lookup(&shell.context.grants, shell.baseline())
        .admit(mooring, shell)?;
    let origin = CommandOrigin::Detached;
    command.with_record(argv, origin, None, mooring, shell, |command, shell| {
        let pid = command
            .prepare(argv, Ownership::Surrendered, shell)?
            .detach(&shell.scope)?;
        Ok(Value::from_datum(Receipt {
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
            diagnosis,
        } = self;
        launch
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Null)
            .stderr(StdioSpec::Null);
        match launch
            .spawn_detached(scope)
            .map_err(|why| diagnosis.explain_start(why))?
        {
            Survivor::Named(pid) => Ok(pid),
            Survivor::Unnamed(why) => Err(Error::new(format!(
                "detach: '{}' is running, but its pid is unknown ({why})",
                diagnosis.shown
            ))
            .into()),
        }
    }
}
