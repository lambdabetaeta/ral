//! `Observe(Register)` — a read of the shell's store, in computation
//! position: what `$CWD`, `$ENV`, and a `~`-path are.
//!
//! The five pseudo-variables were once resolved by name off `Shell` (a
//! stringly-typed `pseudo_var`/`lookup_value_name` pair, reached only after
//! a lexical-scope miss); S8 hoisted every read of them into this typed
//! `Register` form at elaboration time, so this is now their one reader —
//! `Register`'s five variants are the total match, not a string dispatch.

use crate::ir::Register;
use crate::path::tilde::{Unexpandable, expand_tilde_path};
use crate::types::{Error, Shell, Value};
use std::collections::HashMap;

/// A read of the store: the five pseudo-variables, and a `~`-path awaiting
/// `HOME`.  `$SCRIPT` is not among them: the elaborator bakes it to a
/// literal from the file it compiles, so no runtime reader exists.
pub(crate) fn observe(reg: &Register, shell: &Shell) -> Result<Value, Error> {
    match reg {
        Register::Env => Ok(env_map(shell)),
        Register::Args => Ok(Value::list(
            shell.context.args.iter().cloned().map(Value::String).collect(),
        )),
        Register::Nproc => Ok(Value::Int(std::thread::available_parallelism().map_or(
            1,
            |n| {
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "CPU count from available_parallelism is tiny"
                )]
                {
                    n.get() as i64
                }
            },
        ))),
        Register::Cwd => Ok(Value::String(cwd_string(shell))),
        Register::User => Ok(Value::String(
            crate::path::user_name(shell.env_overrides()).unwrap_or_else(|| "?".into()),
        )),
        Register::Tilde(path) => {
            let home = shell.context.home();
            expand_tilde_path(
                path.user.as_deref(),
                path.suffix.as_deref(),
                home.as_deref(),
            )
            .map(Value::String)
            .map_err(|cause| {
                shell.err_hint(
                    format!("cannot resolve {}: {}", path.to_literal(), cause.why()),
                    match cause {
                        Unexpandable::HomeUnknown => "set HOME, or spell out an explicit path",
                        Unexpandable::ForeignUser => {
                            "use bare ~ for the current user, or spell out an explicit path"
                        }
                    },
                    1,
                )
            })
        }
    }
}

/// `$ENV`: the host process environment, overlaid with `within [env: …]`.
/// The host's `PWD` / `OLDPWD` are stale the moment ral `cd`s — the live
/// pair is `context.cwd` — so they are dropped at the source.
fn env_map(shell: &Shell) -> Value {
    let mut merged: HashMap<String, String> = std::env::vars()
        .filter(|(k, _)| !matches!(k.as_str(), "PWD" | "OLDPWD"))
        .collect();
    merged.extend(shell.context.env_overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
    Value::map(
        merged
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect::<Vec<_>>(),
    )
}

/// `$CWD`: total, so an unabbreviatable path names its own placeholder
/// rather than hand back a stand-in the caller could mistake for a real one.
fn cwd_string(shell: &Shell) -> String {
    let p = shell.cwd();
    let home = shell.context.home();
    let cwd_str = crate::path::abbreviate_home(&p, home.as_deref());
    if cwd_str.is_empty() { "?".into() } else { cwd_str }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Register;

    fn new_shell() -> Shell {
        Shell::new(crate::io::TerminalState::default())
    }

    #[test]
    fn cwd_is_live() {
        let shell = new_shell();
        let val = observe(&Register::Cwd, &shell).expect("$CWD must resolve");
        match val {
            Value::String(s) => assert!(!s.is_empty(), "$CWD must be non-empty"),
            other => panic!("$CWD must be a String, got {other:?}"),
        }
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
