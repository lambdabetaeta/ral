use super::util::sig;
use crate::types::*;

/// `cd [path]` — change the shell's working directory.
///
/// Gated by the `shell.chdir` capability; denied when any enclosing `grant`
/// frame restricts shell access without enabling `chdir`.  An empty or
/// missing path means `$HOME`.
///
/// The process cwd is changed synchronously.  Because the `chpwd` lifecycle
/// hook must be fired by the REPL (which owns the plugin runtime), the
/// `(old, new)` pair is stored on `shell.repl.pending_chpwd`; the REPL drains it
/// after the evaluator returns.
pub(super) fn builtin_chdir(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
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
    shell.repl.pending_chpwd = Some((std::path::PathBuf::from(old), std::path::PathBuf::from(new)));
    Ok(Value::Unit)
}
