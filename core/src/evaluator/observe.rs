//! `Observe(Register)` — a read of the shell's store, in computation
//! position: what `$CWD`, `$ENV`, `$HOME` and `~` are.
//!
//! Elaboration hoists every read of a pseudo-variable into this typed
//! `Register` form, so this is their one reader — a total match over
//! `Register`, not a string dispatch.

use crate::ir::Register;
use crate::types::{Error, Shell, Value};

/// A read of the store: a pseudo-variable, or `~` awaiting `HOME`.
/// `$SCRIPT` is not among them: the elaborator bakes it to a literal from
/// the file it compiles, so no runtime reader exists.
pub(crate) fn observe(reg: &Register, shell: &Shell) -> Result<Value, Error> {
    match reg {
        Register::Env => Ok(env_map(shell)),
        Register::Args => Ok(Value::list(
            shell
                .context
                .args
                .iter()
                .cloned()
                .map(Value::string)
                .collect(),
        )),
        Register::Nproc => Ok(Value::Int(
            std::thread::available_parallelism().map_or(1, |n| i64::try_from(n.get()).unwrap_or(1)),
        )),
        Register::Cwd => Ok(Value::string(shell.cwd().to_string_lossy())),
        Register::User => Ok(Value::string(
            crate::path::user_name(shell.env_overrides()).unwrap_or_else(|| "?".into()),
        )),
        Register::Tilde(path) => path
            .expand(shell.context.home().as_deref())
            .map(Value::string)
            .ok_or_else(|| {
                Error::new(
                    format!(
                        "cannot resolve {}: HOME is unset, so `~` names no directory",
                        path.to_literal()
                    ),
                    1,
                )
                .with_hint("set HOME, or spell out an explicit path")
            }),
    }
}

/// `$ENV`: the host process environment, overlaid with `within [env: …]`.
/// The host's `PWD` is stale the moment ral `cd`s — the live one is
/// `context.cwd` — and its `OLDPWD` names the launcher's history, so both are
/// dropped at the source.
fn env_map(shell: &Shell) -> Value {
    let host = std::env::vars().filter(|(k, _)| !matches!(k.as_str(), "PWD" | "OLDPWD"));
    let overrides = shell
        .context
        .env_overrides
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()));
    // `Map`'s `FromIterator` is last-wins, so the overrides come second.
    Value::Map(
        host.chain(overrides)
            .map(|(k, v)| (k, Value::string(v)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Register;

    fn new_shell() -> Shell {
        Shell::new(crate::io::TerminalState::default())
    }

    #[test]
    fn cwd_is_the_logical_cwd() {
        let shell = new_shell();
        let val = observe(&Register::Cwd, &shell).expect("$CWD must resolve");
        assert_eq!(val, Value::string(shell.cwd().to_string_lossy()));
    }

    #[test]
    fn user_is_live() {
        let shell = new_shell();
        let val = observe(&Register::User, &shell).expect("$USER must resolve");
        match val {
            Value::String(s) => assert!(!s.is_empty(), "$USER must be non-empty"),
            other => panic!("$USER must be a String, got {other:?}"),
        }
    }
}
