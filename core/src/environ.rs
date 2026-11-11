//! Environment variables in a ral session, and the environment an external
//! command is launched with.
//!
//! A shell does not modify its process's environment. Variables set by
//! `within [env: …]`, the rc file or an embedding host are kept in an
//! [`EnvVars`] instead. A lookup ([`EnvVars::get_or_host`]) checks it first,
//! then the process environment, read through [`crate::host`].
//!
//! `PWD` and `OLDPWD` are [`Reserved`]. ral keeps its working directory in
//! the shell, since `cd` never calls `chdir`, and keeps no previous directory.
//! So `EnvVars` refuses to bind either name, and a lookup never returns the
//! process's stale value.
//!
//! [`EnvVars::visible`] is what the `env` builtin returns.
//! [`EnvVars::for_child`] gives the [`ChildEnv`] for launching an external
//! command:
//! - the overrides are set;
//! - `PWD` is set to the launch directory;
//! - `OLDPWD` is removed;
//! - if a grant is active, the dynamic-loader variables (`LD_PRELOAD`,
//!   `LD_AUDIT`, `LD_LIBRARY_PATH`, `DYLD_*`) are removed. Any of them could
//!   make the admitted program load code the grant never admitted; on macOS
//!   they would reach the re-exec before it enters Seatbelt.

use crate::host::{self, HOME_VARS, USER_VARS, first_bound};
use crate::path::CwdPath;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;

/// A variable ral manages itself, so no overlay may bind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserved {
    Pwd,
    Oldpwd,
}

impl Reserved {
    const ALL: [Self; 2] = [Self::Pwd, Self::Oldpwd];

    /// `name`, if it is reserved. Windows compares variable names without
    /// regard to ASCII case.
    pub fn of(name: &str) -> Option<Self> {
        let spells = |reserved: Self| {
            if cfg!(windows) {
                reserved.name().eq_ignore_ascii_case(name)
            } else {
                reserved.name() == name
            }
        };
        Self::ALL.into_iter().find(|&reserved| spells(reserved))
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Pwd => "PWD",
            Self::Oldpwd => "OLDPWD",
        }
    }

    const fn why(self) -> &'static str {
        match self {
            Self::Pwd => {
                "is the shell's working directory; use `cd` or `within [dir: …]` to change it"
            }
            Self::Oldpwd => {
                "is never passed to commands: ral keeps no previous directory for `cd -`"
            }
        }
    }
}

impl fmt::Display for Reserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` {}", self.name(), self.why())
    }
}

impl std::error::Error for Reserved {}

const LOADER_VARS: [&str; 3] = ["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"];

/// Every `DYLD_` variable belongs to the macOS loader.
const LOADER_PREFIX: &str = "DYLD_";

fn is_loader_var(name: &OsStr) -> bool {
    LOADER_VARS.iter().any(|var| name == *var)
        || name
            .as_encoded_bytes()
            .starts_with(LOADER_PREFIX.as_bytes())
}

/// Env-var overrides over the host environment. Cheap to clone; never binds
/// a [`Reserved`] name, deserialisation included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "imbl::HashMap<String, String>")]
pub struct EnvVars(imbl::HashMap<String, String>);

impl TryFrom<imbl::HashMap<String, String>> for EnvVars {
    type Error = Reserved;

    fn try_from(map: imbl::HashMap<String, String>) -> Result<Self, Reserved> {
        let reserved = map.keys().find_map(|key| Reserved::of(key));
        reserved.map_or(Ok(Self(map)), Err)
    }
}

impl EnvVars {
    pub fn new() -> Self {
        Self(imbl::HashMap::new())
    }

    /// # Errors
    /// A key is [`Reserved`].
    pub fn from_pairs<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> Result<Self, Reserved>
    where
        K: Into<String>,
        V: Into<String>,
    {
        let mut env = Self::new();
        for (key, value) in pairs {
            env.insert(key.into(), value.into())?;
        }
        Ok(env)
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    /// `key` from the overlay, else from the host. A non-UTF-8 host value
    /// and a [`Reserved`] name read as unbound.
    pub(crate) fn get_or_host(&self, key: &str) -> Option<String> {
        if Reserved::of(key).is_some() {
            return None;
        }
        self.get(key).cloned().or_else(|| host::var(key))
    }

    /// `HOME`, then `USERPROFILE`: each key overlay first, then the host.
    pub fn home(&self) -> Option<String> {
        first_bound(&HOME_VARS, |k| self.get_or_host(k))
    }

    /// `USER`, then `USERNAME`, as [`Self::home`].
    pub fn user(&self) -> Option<String> {
        first_bound(&USER_VARS, |k| self.get_or_host(k))
    }

    /// # Errors
    /// `key` is [`Reserved`].
    pub(crate) fn insert(&mut self, key: String, value: String) -> Result<(), Reserved> {
        if let Some(reserved) = Reserved::of(&key) {
            return Err(reserved);
        }
        self.0.insert(key, value);
        Ok(())
    }

    /// Merge `over` in; its bindings win.
    pub fn overlay(&mut self, over: &Self) {
        self.0 = over.0.clone().union(std::mem::take(&mut self.0));
    }

    pub fn iter(&self) -> EnvVarsIter<'_> {
        EnvVarsIter(self.0.iter())
    }

    /// The host's UTF-8 variables under this overlay, without the
    /// [`Reserved`] ones.
    pub fn visible(&self) -> impl Iterator<Item = (String, String)> + '_ {
        let inherited = host::vars_os()
            .into_iter()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| Reserved::of(k).is_none() && !self.0.contains_key(k));
        inherited.chain(self.iter().map(|(k, v)| (k.clone(), v.clone())))
    }

    /// The environment of a child launched in `cwd`. `restricted` (a grant
    /// is active) strips the loader variables.
    pub(crate) fn for_child<'a>(&'a self, cwd: &'a CwdPath, restricted: bool) -> ChildEnv<'a> {
        let admitted = |name: &OsStr| !(restricted && is_loader_var(name));
        let mut set: Vec<(&OsStr, &OsStr)> = self
            .iter()
            .map(|(k, v)| (OsStr::new(k), OsStr::new(v)))
            .filter(|(k, _)| admitted(k))
            .collect();
        set.push((OsStr::new(Reserved::Pwd.name()), cwd.as_path().as_os_str()));
        // Host loader variables would be inherited.
        let mut unset = vec![Cow::Borrowed(OsStr::new(Reserved::Oldpwd.name()))];
        unset.extend(
            host::vars_os()
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| !admitted(k))
                .map(Cow::Owned),
        );
        ChildEnv {
            cwd: cwd.as_path(),
            set,
            unset,
        }
    }
}

/// A child's cwd and its edits to the inherited environment. The cwd is
/// carried here so that it and `PWD` come from one place. `set` and `unset`
/// are disjoint. Everything but the host's loader variables is borrowed.
#[derive(Debug)]
pub(crate) struct ChildEnv<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) set: Vec<(&'a OsStr, &'a OsStr)>,
    pub(crate) unset: Vec<Cow<'a, OsStr>>,
}

pub struct EnvVarsIter<'a>(
    imbl::hashmap::Iter<'a, String, String, imbl::shared_ptr::DefaultSharedPtr>,
);

impl<'a> Iterator for EnvVarsIter<'a> {
    type Item = (&'a String, &'a String);
    fn next(&mut self) -> Option<(&'a String, &'a String)> {
        self.0.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'a> IntoIterator for &'a EnvVars {
    type Item = (&'a String, &'a String);
    type IntoIter = EnvVarsIter<'a>;
    fn into_iter(self) -> EnvVarsIter<'a> {
        EnvVarsIter(self.0.iter())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::host::test_support::{with_var, with_vars_cleared};
    use crate::test_helper::process_cwd;

    fn env_of(pairs: &[(&str, &str)]) -> EnvVars {
        EnvVars::from_pairs(pairs.iter().copied()).unwrap()
    }

    #[test]
    fn overlays_shadow_the_host_without_mutating_it_or_other_clones() {
        with_var("RAL_ENV_TEST_VAR", Some("host"), || {
            let mut env = EnvVars::new();
            assert_eq!(env.get("RAL_ENV_TEST_VAR"), None);
            assert_eq!(env.get_or_host("RAL_ENV_TEST_VAR").as_deref(), Some("host"));
            env.insert("RAL_ENV_TEST_VAR".into(), "overlay".into())
                .unwrap();

            let mut child = env.clone();
            child.overlay(&env_of(&[
                ("RAL_ENV_TEST_VAR", "first"),
                ("RAL_ENV_TEST_VAR", "last"),
            ]));
            assert_eq!(
                child.get_or_host("RAL_ENV_TEST_VAR").as_deref(),
                Some("last")
            );
            child
                .insert("RAL_ENV_TEST_VAR".into(), String::new())
                .unwrap();
            assert_eq!(child.get_or_host("RAL_ENV_TEST_VAR").as_deref(), Some(""));
            assert_eq!(
                env.get_or_host("RAL_ENV_TEST_VAR").as_deref(),
                Some("overlay")
            );
            assert_eq!(host::var("RAL_ENV_TEST_VAR").as_deref(), Some("host"));
        });
    }

    #[test]
    fn no_overlay_holds_a_reserved_name() {
        let mut overlay = env_of(&[("A", "kept")]);
        for reserved in Reserved::ALL {
            let name = reserved.name();
            assert_eq!(Reserved::of(name), Some(reserved));
            assert_eq!(overlay.insert(name.into(), "x".into()), Err(reserved));
            assert_eq!(
                EnvVars::from_pairs([("B", "x"), (name, "x")]).err(),
                Some(reserved)
            );
            let json = serde_json::json!({ name: "x" });
            assert!(serde_json::from_value::<EnvVars>(json).is_err());
        }
        assert_eq!(overlay, env_of(&[("A", "kept")]));
        assert_eq!(Reserved::of("pwd").is_some(), cfg!(windows));
    }

    #[test]
    fn a_reserved_name_reads_as_unbound_whatever_the_host_binds() {
        with_var("OLDPWD", Some("/launcher/previous"), || {
            let overlay = EnvVars::new();
            assert_eq!(overlay.get_or_host("OLDPWD"), None);
            assert_eq!(overlay.get_or_host("PWD"), None);
            assert!(overlay.visible().all(|(k, _)| Reserved::of(&k).is_none()));
        });
    }

    #[test]
    fn the_visible_environment_names_each_variable_once_overlay_winning() {
        with_var("RAL_ENV_TEST_VISIBLE", Some("host"), || {
            let overlay = env_of(&[("RAL_ENV_TEST_VISIBLE", "overlay")]);
            let shown: Vec<_> = overlay
                .visible()
                .filter(|(k, _)| k == "RAL_ENV_TEST_VISIBLE")
                .collect();
            assert_eq!(shown, [("RAL_ENV_TEST_VISIBLE".into(), "overlay".into())]);
        });
    }

    fn sets<'a>(child: &ChildEnv<'a>, name: &str) -> Option<&'a OsStr> {
        let found = child.set.iter().find(|(k, _)| *k == name);
        found.map(|(_, v)| *v)
    }

    fn unsets(child: &ChildEnv<'_>, name: &str) -> bool {
        child.unset.iter().any(|k| *k == OsStr::new(name))
    }

    #[test]
    fn a_child_is_told_its_directory_and_no_previous_one() {
        let (cwd, overlay) = (process_cwd(), env_of(&[("A", "a")]));
        let child = overlay.for_child(&cwd, false);
        assert_eq!(child.cwd, cwd.as_path());
        assert_eq!(sets(&child, "PWD"), Some(cwd.as_path().as_os_str()));
        assert_eq!(sets(&child, "A"), Some(OsStr::new("a")));
        assert!(unsets(&child, "OLDPWD"));
    }

    #[test]
    fn a_restricted_child_receives_no_loader_variable_from_host_or_overlay() {
        // Concurrent tests' children inherit the host variable: no loader reads it.
        const HOST: &str = "DYLD_RAL_TEST_OPTION";
        with_var(HOST, Some("v"), || {
            let cwd = process_cwd();
            let overlay = env_of(&[
                ("LD_PRELOAD", "overlay.so"),
                ("DYLD_FUTURE_OPTION", "v"),
                ("OTHER_OPTION", "v"),
            ]);

            let ambient = overlay.for_child(&cwd, false);
            assert_eq!(sets(&ambient, "LD_PRELOAD"), Some(OsStr::new("overlay.so")));
            assert!(!unsets(&ambient, HOST));

            let restricted = overlay.for_child(&cwd, true);
            for loader in ["LD_PRELOAD", "DYLD_FUTURE_OPTION", HOST] {
                assert_eq!(sets(&restricted, loader), None, "{loader} is not set");
            }
            assert!(unsets(&restricted, HOST));
            assert_eq!(sets(&restricted, "OTHER_OPTION"), Some(OsStr::new("v")));
            assert!(
                restricted
                    .unset
                    .iter()
                    .all(|k| restricted.set.iter().all(|(s, _)| *s != &**k)),
                "no name is both set and unset"
            );
        });
    }

    #[test]
    fn home_checks_each_key_overlay_then_host_and_skips_empty_bindings() {
        with_vars_cleared(&["HOME", "USERPROFILE"], || {
            let mut env = EnvVars::new();
            assert_eq!(env.home(), None);
            // SAFETY: the helper holds the environment guard and restores both keys.
            unsafe { std::env::set_var("USERPROFILE", "host-profile") };
            assert_eq!(env.home().as_deref(), Some("host-profile"));
            env.insert("USERPROFILE".into(), "overlay-profile".into())
                .unwrap();
            assert_eq!(env.home().as_deref(), Some("overlay-profile"));
            // SAFETY: the same guard covers HOME.
            unsafe { std::env::set_var("HOME", "host-home") };
            assert_eq!(env.home().as_deref(), Some("host-home"));
            env.insert("HOME".into(), "overlay-home".into()).unwrap();
            assert_eq!(env.home().as_deref(), Some("overlay-home"));
            env.insert("HOME".into(), String::new()).unwrap();
            assert_eq!(env.home().as_deref(), Some("overlay-profile"));
            env.insert("USERPROFILE".into(), String::new()).unwrap();
            assert_eq!(env.home(), None);
        });
    }
}

impl From<Reserved> for crate::fact::ReservedVar {
    fn from(value: Reserved) -> Self {
        match value {
            Reserved::Pwd => Self::Pwd,
            Reserved::Oldpwd => Self::Oldpwd,
        }
    }
}

impl EnvVars {
    pub(crate) fn from_json_map(
        map: imbl::HashMap<String, String>,
    ) -> Result<Self, crate::fact::DecodeFailure> {
        if let Some((name, reserved)) = map
            .keys()
            .find_map(|key| Reserved::of(key).map(|reserved| (key.clone(), reserved)))
        {
            return Err(crate::fact::DecodeFailure::new(
                crate::fact::DecodeCause::ReservedEnvironment(
                    crate::fact::ReservedEnvironmentFailure {
                        name,
                        reserved: reserved.into(),
                    },
                ),
            ));
        }
        Ok(Self(map))
    }
}

impl crate::data::json::JsonDecode for EnvVars {
    fn decode_json(
        value: crate::data::json::JsonRef<'_, '_>,
    ) -> Result<Self, crate::fact::DecodeFailure> {
        Self::from_json_map(crate::data::json::decode_structural(value)?)
    }
}

impl crate::data::json::JsonEncode for EnvVars {
    fn encode_json(
        &self,
        format: crate::data::json::JsonFormat,
    ) -> Result<Vec<u8>, crate::fact::JsonEncodeFailure> {
        Ok(crate::data::json::serialize_structural(&self.0, format))
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;
    use crate::data::json::{from_slice, to_vec};
    use crate::fact::{DecodeCause, JsonInputFailure};

    #[test]
    fn typed_env_json_validates_after_map_replacement() {
        let env = from_slice::<EnvVars>(br#"{"A":"first","A":"last","EMPTY":""}"#).unwrap();
        assert_eq!(env.get("A").map(String::as_str), Some("last"));
        assert_eq!(from_slice::<EnvVars>(&to_vec(&env).unwrap()).unwrap(), env);
        for key in ["PWD", "OLDPWD"] {
            let bytes = format!(r#"{{"{key}":"x"}}"#);
            let JsonInputFailure::Decode(error) =
                from_slice::<EnvVars>(bytes.as_bytes()).unwrap_err()
            else {
                panic!("syntax")
            };
            assert!(
                matches!(*error.cause, DecodeCause::ReservedEnvironment(cause) if cause.name == key)
            );
        }
        assert_eq!(
            from_slice::<EnvVars>(br#"{"pwd":"x"}"#).is_err(),
            cfg!(windows)
        );
    }
}
