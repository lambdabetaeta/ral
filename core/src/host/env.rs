//! Persistent process-environment overrides, with host fallback on reads.

use serde::{Deserialize, Serialize};

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod test_support;

/// Persistent string→string map of env-var overrides, cheap to clone.
///
/// `Serialize` / `Deserialize` preserve the overlay across seed IPC boundaries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvVars(imbl::HashMap<String, String>);

impl EnvVars {
    pub fn new() -> Self {
        Self(imbl::HashMap::new())
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    /// Look up `key`, this override map first, then the host process env.  The
    /// one home of that fallback: no caller should spell it out again.  A host
    /// value that is not UTF-8 reads as unbound, as in [`Self::host_text`].
    pub(crate) fn get_or_host(&self, key: &str) -> Option<String> {
        self.get(key).cloned().or_else(|| crate::host::var(key))
    }

    /// `HOME`, then `USERPROFILE`: each key overlay first, then the host.
    pub fn home(&self) -> Option<String> {
        crate::host::first_bound(&crate::host::HOME_VARS, |k| self.get_or_host(k))
    }

    /// `USER`, then `USERNAME`, as [`Self::home`].
    pub fn user(&self) -> Option<String> {
        crate::host::first_bound(&crate::host::USER_VARS, |k| self.get_or_host(k))
    }

    /// The host process env as ral reads it: text only.  A pair that is not
    /// UTF-8 is left out, never mangled; children still inherit its bytes.
    pub(crate) fn host_text() -> impl Iterator<Item = (String, String)> {
        crate::host::vars_os()
            .into_iter()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
    }

    pub(crate) fn insert(&mut self, key: String, value: String) -> Option<String> {
        self.0.insert(key, value)
    }

    pub fn iter(&self) -> EnvVarsIter<'_> {
        EnvVarsIter(self.0.iter())
    }
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

impl<K, V> Extend<(K, V)> for EnvVars
where
    K: Into<String>,
    V: Into<String>,
{
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.0.insert(k.into(), v.into());
        }
    }
}

impl<K, V> FromIterator<(K, V)> for EnvVars
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = imbl::HashMap::new();
        for (k, v) in iter {
            m.insert(k.into(), v.into());
        }
        Self(m)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::host::env::test_support::{with_var, with_vars_cleared};
    use std::collections::BTreeMap;

    #[test]
    fn overlays_shadow_the_host_without_mutating_it_or_other_clones() {
        with_var("RAL_ENV_TEST_VAR", Some("host"), || {
            let mut env = EnvVars::new();
            assert_eq!(env.get("RAL_ENV_TEST_VAR"), None);
            assert_eq!(env.get_or_host("RAL_ENV_TEST_VAR").as_deref(), Some("host"));
            assert_eq!(
                env.insert("RAL_ENV_TEST_VAR".into(), "overlay".into()),
                None
            );

            let mut child = env.clone();
            child.extend([("RAL_ENV_TEST_VAR", "first"), ("RAL_ENV_TEST_VAR", "last")]);
            assert_eq!(
                child.get_or_host("RAL_ENV_TEST_VAR").as_deref(),
                Some("last")
            );
            assert_eq!(
                child.insert("RAL_ENV_TEST_VAR".into(), String::new()),
                Some("last".into())
            );
            assert_eq!(child.get_or_host("RAL_ENV_TEST_VAR").as_deref(), Some(""));
            assert_eq!(
                env.get_or_host("RAL_ENV_TEST_VAR").as_deref(),
                Some("overlay")
            );
            assert_eq!(
                crate::host::var("RAL_ENV_TEST_VAR").as_deref(),
                Some("host")
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
            env.extend([("USERPROFILE", "overlay-profile")]);
            assert_eq!(env.home().as_deref(), Some("overlay-profile"));
            // SAFETY: the same guard covers HOME.
            unsafe { std::env::set_var("HOME", "host-home") };
            assert_eq!(env.home().as_deref(), Some("host-home"));
            env.extend([("HOME", "overlay-home")]);
            assert_eq!(env.home().as_deref(), Some("overlay-home"));
            env.extend([("HOME", "")]);
            assert_eq!(env.home().as_deref(), Some("overlay-profile"));
            env.extend([("USERPROFILE", "")]);
            assert_eq!(env.home(), None);
        });
    }

    #[test]
    fn collection_and_iteration_keep_the_last_value_for_each_key() {
        let env: EnvVars = [("A", "first"), ("B", "kept"), ("A", "last")]
            .into_iter()
            .collect();
        let expected = BTreeMap::from([("A".into(), "last".into()), ("B".into(), "kept".into())]);
        let iter: EnvVarsIter<'_> = (&env).into_iter();
        assert_eq!(iter.size_hint(), env.iter().size_hint());
        assert_eq!(
            iter.map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            expected
        );
        assert_eq!(
            env.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            expected
        );
    }

    #[test]
    fn serde_is_a_plain_string_map_including_empty_values() {
        let json = serde_json::json!({"HOME": "/overlay", "EMPTY": ""});
        let env: EnvVars = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(env.get("HOME").map(String::as_str), Some("/overlay"));
        assert_eq!(env.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(serde_json::to_value(&env).unwrap(), json);
        assert_eq!(
            serde_json::to_value(EnvVars::default()).unwrap(),
            serde_json::json!({})
        );
        let empty: EnvVars = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty.iter().count(), 0);
    }
}
