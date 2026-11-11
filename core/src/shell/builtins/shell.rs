//! The shell-state builtins: `alias`, `unalias`, and `cd`.

use crate::authority::policy::{Flag, ShellVerb};
use crate::error::Error;
use crate::fact::{
    ArityFailure, CountRule, Expected, InvalidCause, Reason, StateFailure, ValueShape,
};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Settled, Shell, Value};

/// `alias NAME BODY` — the interactive door to [`Shell::install_alias`],
/// which the rc `aliases:` map and the plugin loader also come in through.
pub(super) fn builtin_alias(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let (name, thunk) = match args {
        [Value::String(name), thunk @ Value::Thunk(_)] => (name.to_string(), thunk.clone()),
        [Value::String(_), other] => {
            return Err(other.expected("alias", Expected::Closure).into());
        }
        [other, _] => {
            return Err(other
                .expected("alias", Expected::Shape(ValueShape::String))
                .into());
        }
        _ => {
            return Err(invalid(
                "alias",
                InvalidCause::Arity(ArityFailure {
                    expected: CountRule::Exactly(2usize.into()),
                    got: args.len().into(),
                }),
            )
            .into());
        }
    };
    shell.install_alias(name, thunk)?;
    Ok(Value::Unit)
}

/// `unalias NAME` — remove the alias for `NAME`, erroring if none is installed.
pub(super) fn builtin_unalias(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let name = match args {
        [Value::String(s)] => s.as_str(),
        [other] => {
            return Err(other
                .expected("unalias", Expected::Shape(ValueShape::String))
                .into());
        }
        _ => {
            return Err(invalid(
                "unalias",
                InvalidCause::Arity(ArityFailure {
                    expected: CountRule::Exactly(1usize.into()),
                    got: args.len().into(),
                }),
            )
            .into());
        }
    };
    if !shell.remove_alias(name) {
        return Err(
            Error::new(Reason::State(Box::new(StateFailure::AliasMissing(
                name.into(),
            ))))
            .into(),
        );
    }
    Ok(Value::Unit)
}

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
