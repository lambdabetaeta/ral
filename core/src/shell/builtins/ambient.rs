//! The ambient reads — the dynamic `Context` and the machine — as nullary natives.

use crate::fact::{AmbientSetting, Reason, StateFailure};
use crate::shell::runtime::{Error, Mooring, Settled, Shell, ValClos};
use crate::ty::Site;
use std::sync::Arc;

pub(super) fn cwd(shell: &Shell) -> ValClos {
    ValClos::string(crate::path::shown(shell.cwd()))
}

/// Home as ral shows it: the spelling `~` expands to
/// ([`crate::path::TildePath::shown`]), so `~/x` and `"!{home}/x"` are
/// the same string on every host.
pub(super) fn home(shell: &Shell) -> Result<ValClos, Error> {
    let home = shell.context.require_home()?;
    Ok(ValClos::string(crate::path::shown(home)))
}

/// The host environment under the overlay:
/// [`EnvVars::visible`](crate::environ::EnvVars::visible).
pub(super) fn env(shell: &Shell) -> ValClos {
    ValClos::map(
        shell
            .context
            .env_overrides()
            .visible()
            .map(|(k, v)| (k, ValClos::string(v)))
            .collect(),
    )
}

pub(super) fn builtin_env(
    _args: &[ValClos],
    site: &Arc<Site>,
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    site.admitted("env", env(shell), shell)
}

pub(super) fn args(shell: &Shell) -> ValClos {
    ValClos::list(
        shell
            .context
            .args
            .iter()
            .cloned()
            .map(ValClos::string)
            .collect(),
    )
}

pub(super) fn user(shell: &Shell) -> Result<ValClos, Error> {
    shell
        .env_overrides()
        .user()
        .map(ValClos::string)
        .ok_or_else(|| {
            Error::new(Reason::State(Box::new(StateFailure::MissingAmbient(
                AmbientSetting::User,
            ))))
        })
}

pub(super) fn nproc() -> ValClos {
    ValClos::int(
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
    fn env_has_neither_pwd_nor_oldpwd() {
        let value = env(&new_shell());
        let m = value.as_map("env").expect("env must be a Map");
        assert!(m.get("PWD").is_none() && m.get("OLDPWD").is_none());
    }

    #[test]
    fn env_reads_the_override() {
        let mut shell = new_shell();
        shell
            .context
            .set_env_var("RAL_AMBIENT_TEST_KEY", "v")
            .unwrap();
        let value = env(&shell);
        let m = value.as_map("env").expect("env must be a Map");
        assert_eq!(
            m.get("RAL_AMBIENT_TEST_KEY").as_deref(),
            Some(&ValClos::string("v"))
        );
    }

    #[test]
    fn absent_user_is_a_missing_ambient_setting() {
        let shell = new_shell();
        crate::host::test_support::with_vars_cleared(&["USER", "USERNAME"], || {
            assert_eq!(
                user(&shell).unwrap_err().reason,
                Reason::State(Box::new(StateFailure::MissingAmbient(AmbientSetting::User)))
            );
        });
    }
}
