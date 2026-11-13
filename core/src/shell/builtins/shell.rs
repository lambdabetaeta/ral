//! The shell-state builtin: `cd`.

use crate::authority::policy::{Flag, ShellVerb};
use crate::fact::{Expected, InvalidCause, ValueShape};
use crate::ir::{ValClos, View};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Settled, Shell};

/// `cd <path>` — move the shell's logical cwd, never the OS process cwd.
pub(super) fn builtin_chdir(args: &[ValClos], shell: &mut Shell) -> Settled<ValClos> {
    // The checker guarantees one String, but not a non-empty one, and
    // `resolve_path` would read "" as the cwd: making `cd $d` a silent
    // success that moved nowhere.
    let path = match args[0].view() {
        View::String(s) if s.is_empty() => {
            return Err(invalid("cd", InvalidCause::EmptyPath).into());
        }
        View::String(s) => s.as_str(),
        _ => {
            return Err(args[0]
                .expected("cd", Expected::Shape(ValueShape::String))
                .into());
        }
    };

    shell.check(Flag::Shell(ShellVerb::Chdir), "cd")?;
    shell.apply_chdir(path)?;
    Ok(ValClos::UNIT)
}
