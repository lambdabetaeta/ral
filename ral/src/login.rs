//! Adopt only changes made by the POSIX login profiles.

use std::collections::HashMap;

/// # Safety
/// No other thread exists: this sets the process environment.
pub(crate) unsafe fn adopt_posix_profile() {
    let before: HashMap<_, _> = ral_core::host::vars_os().into_iter().collect();
    let after = match ral_core::host::login_env::collect() {
        Ok(after) => after.into_iter().collect::<HashMap<_, _>>(),
        Err(why) => {
            ral_core::errln!(
                "ral: the login environment from /etc/profile and ~/.profile was not adopted: {why}; continuing with the environment ral was started with"
            );
            return;
        }
    };
    let bookkeeping =
        |key: &std::ffi::OsStr| ["SHLVL", "PWD", "OLDPWD", "_"].iter().any(|k| key == *k);
    for (key, value) in &after {
        if !bookkeeping(key) && before.get(key) != Some(value) {
            // SAFETY: the caller guarantees exclusive process environment access.
            unsafe { std::env::set_var(key, value) };
        }
    }
    for key in before.keys() {
        if !bookkeeping(key) && !after.contains_key(key) {
            // SAFETY: the caller guarantees exclusive process environment access.
            unsafe { std::env::remove_var(key) };
        }
    }
}
