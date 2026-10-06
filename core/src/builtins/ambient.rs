//! The ambient reads — the dynamic `Context` and the machine — as nullary natives.

use crate::types::{EnvVars, Error, Shell, Value};

pub(super) fn cwd(shell: &Shell) -> Value {
    Value::string(shell.cwd().to_string_lossy())
}

/// The host process environment, overlaid with `within [env: …]`.
/// The host's `PWD` is stale the moment ral `cd`s — the live one is
/// `context.cwd` — and its `OLDPWD` names the launcher's history, so both are
/// dropped at the source.
pub(super) fn env(shell: &Shell) -> Value {
    let host = EnvVars::host_text().filter(|(k, _)| !matches!(k.as_str(), "PWD" | "OLDPWD"));
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

pub(super) fn args(shell: &Shell) -> Value {
    Value::list(
        shell
            .context
            .args
            .iter()
            .cloned()
            .map(Value::string)
            .collect(),
    )
}

pub(super) fn user(shell: &Shell) -> Result<Value, Error> {
    shell
        .env_overrides()
        .user()
        .map(Value::string)
        .ok_or_else(|| {
            Error::new("`user` needs USER (or USERNAME on Windows), and neither is set")
                .with_hint("set USER, e.g. `within [env: [USER: \"me\"]] { … }`")
        })
}

pub(super) fn nproc() -> Value {
    Value::Int(
        std::thread::available_parallelism().map_or(1, |n| i64::try_from(n.get()).unwrap_or(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_shell() -> Shell {
        crate::test_helper::core_shell()
    }

    #[test]
    fn cwd_is_the_logical_cwd() {
        let shell = new_shell();
        assert_eq!(cwd(&shell), Value::string(shell.cwd().to_string_lossy()));
    }

    #[test]
    fn user_is_live() {
        match user(&new_shell()) {
            Ok(Value::String(s)) => assert!(!s.is_empty(), "user must be non-empty"),
            Ok(other) => panic!("user must be a String, got {other:?}"),
            Err(_) => {}
        }
    }

    #[test]
    fn user_reads_the_override() {
        let mut shell = new_shell();
        shell.context.set_env_var("USER", "bob");
        assert_eq!(user(&shell).unwrap(), Value::string("bob"));
    }

    #[test]
    fn env_has_neither_pwd_nor_oldpwd() {
        let Value::Map(m) = env(&new_shell()) else {
            panic!("env must be a Map");
        };
        assert!(m.get("PWD").is_none() && m.get("OLDPWD").is_none());
    }

    #[test]
    fn env_reads_the_override() {
        let mut shell = new_shell();
        shell.context.set_env_var("RAL_AMBIENT_TEST_KEY", "v");
        let Value::Map(m) = env(&shell) else {
            panic!("env must be a Map");
        };
        assert_eq!(
            m.get("RAL_AMBIENT_TEST_KEY").as_deref(),
            Some(&Value::string("v"))
        );
    }

    #[test]
    fn nproc_is_positive() {
        assert!(matches!(nproc(), Value::Int(n) if n >= 1));
    }
}
