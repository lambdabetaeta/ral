//! The shell-state builtin: `cd`.

use crate::authority::policy::{Flag, ShellVerb};
use crate::fact::{Expected, InvalidCause, ValueShape};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Settled, Shell, Value};

/// `cd <path>` — move the shell's logical cwd, never the OS process cwd.
pub(super) fn builtin_chdir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    // The checker guarantees one String, but not a non-empty one, and
    // `resolve_path` would read "" as the cwd: making `cd $d` a silent
    // success that moved nowhere.
    let path = match &args[0] {
        Value::String(s) if s.is_empty() => {
            return Err(invalid("cd", InvalidCause::EmptyPath).into());
        }
        Value::String(s) => s.as_str(),
        other => {
            return Err(other
                .expected("cd", Expected::Shape(ValueShape::String))
                .into());
        }
    };

    shell.check(Flag::Shell(ShellVerb::Chdir), "cd")?;
    shell.apply_chdir(path)?;
    Ok(Value::Unit)
}
