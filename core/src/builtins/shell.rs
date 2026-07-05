use crate::types::*;

/// `alias NAME BODY` — install a per-name handler frame for `NAME`
/// that runs `BODY` when the name is invoked.  Replaces any prior alias
/// for the same name; never affects `within` frames.  Returns `Unit`.
///
/// Same write side as the rc-config / plugin-loader alias installers —
/// the interactive form here and the data-driven loaders both call
/// [`Shell::install_alias`].
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

/// `unalias NAME` — remove a previously installed alias.  Errors if no
/// alias for `NAME` is installed; never touches `within` frames.
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

/// `cd [path]` — change the shell's working directory.
///
/// Gated by the `shell.chdir` capability; denied when any enclosing `grant`
/// frame restricts shell access without enabling `chdir`.  An empty or
/// missing path means `$HOME`.
///
/// Only the shell-owned `Cwd` is updated, synchronously; the OS process cwd is left untouched.  Because the `chpwd` lifecycle
/// hook must be fired by the REPL (which owns the plugin runtime), the
/// `(old, new)` pair is stored on `shell.local.repl.pending_chpwd`; the REPL drains it
/// after the evaluator returns.
pub(super) fn builtin_chdir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let path = match args.first() {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(sig(format!(
                "cd: expected a String path, got {}",
                other.type_name()
            )));
        }
        None => String::new(),
    };

    shell.check_shell_chdir()?;
    let (old, new) = shell.apply_chdir(&path)?;
    // String→PathBuf for the chpwd notification queue.  No path
    // semantics in play — just storing the already-resolved cwd
    // strings as paths for the listener.
    #[allow(clippy::disallowed_methods)]
    let pending = (std::path::PathBuf::from(old), std::path::PathBuf::from(new));
    shell.local.repl.pending_chpwd = Some(pending);
    Ok(Value::Unit)
}
