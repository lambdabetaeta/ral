//! The shell-state builtins: `alias`, `unalias`, and `cd`.

use crate::types::{Settled, Shell, Value, sig};

/// `alias NAME BODY` — the interactive door to [`Shell::install_alias`],
/// which the rc `aliases:` map and the plugin loader also come in through.
pub(super) fn builtin_alias(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let (name, thunk) = match args {
        [
            Value::String(name),
            thunk @ (Value::Lambda { .. } | Value::Block { .. }),
        ] => (name.clone(), thunk.clone()),
        [Value::String(_), other] => {
            return Err(sig(format!(
                "alias: body must be a block, got {}",
                other.type_name()
            )));
        }
        [other, _] => {
            return Err(sig(format!(
                "alias: name must be a String, got {}",
                other.type_name()
            )));
        }
        _ => {
            return Err(sig(format!(
                "alias: expected `alias NAME {{ |args| BODY }}`, got {} args",
                args.len()
            )));
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
            return Err(sig(format!(
                "unalias: name must be a String, got {}",
                other.type_name()
            )));
        }
        _ => {
            return Err(sig(format!(
                "unalias: expected `unalias NAME`, got {} args",
                args.len()
            )));
        }
    };
    if !shell.remove_alias(name) {
        return Err(sig(format!("unalias: no alias named '{name}'")));
    }
    Ok(Value::Unit)
}

/// `cd <path>` — move the shell's logical cwd, never the OS process cwd.
///
/// Only the REPL owns the plugin runtime that fires `chpwd`, so the
/// `(old, new)` pair waits on `local.repl.pending_chpwd` for it to drain.
pub(super) fn builtin_chdir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    // The checker guarantees one String, but not a non-empty one, and
    // `resolve_path` would read "" as the cwd — making `cd $d` a silent
    // success that moved nowhere.
    let path = match &args[0] {
        Value::String(s) if s.is_empty() => {
            return Err(sig(
                "cd: the empty string names no directory — did you mean `cd .`, or `cd ~`?"
                    .to_string(),
            ));
        }
        Value::String(s) => s.clone(),
        other => {
            return Err(sig(format!(
                "cd: expected a String path, got {}",
                other.type_name()
            )));
        }
    };

    shell.check_shell_chdir()?;
    let (old, new) = shell.apply_chdir(&path)?;
    shell.local.repl.pending_chpwd =
        Some((std::path::PathBuf::from(old), std::path::PathBuf::from(new)));
    Ok(Value::Unit)
}
