//! The models the human bound to names at launch for spawned agents.

use crate::provider::models::{ListFailure, Listed};
use crate::provider::state::{Pair, named};
use crate::provider::{Account, Bureau, Provider, Tuning};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The providers the human bound to names at launch for spawned agents.
#[derive(Default)]
pub struct Roles(pub(crate) BTreeMap<String, Arc<Provider>>);

/// Split `NAME=VALUE` at the first `=`; a clap value parser.
///
/// # Errors
/// If either side is empty or there is no `=`.
pub fn binding(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(name, value)| !name.is_empty() && !value.is_empty())
        .map(|(name, value)| (name.into(), value.into()))
        .ok_or_else(|| format!("expected NAME=VALUE, got '{s}'"))
}

/// Each `--role` resolved to its account and model, on its `--role-provider`
/// account when one is given.
///
/// # Errors
/// If a role or a `--role-provider` is given twice, if a `--role-provider`
/// names no role, or if a role's model resolves to no single account.
pub(crate) fn bindings(
    roles: &[(String, String)],
    providers: &[(String, String)],
    available: &[Account],
    mut listing: impl FnMut(&Account) -> Result<Vec<Listed>, ListFailure>,
) -> Result<Vec<(String, Pair)>, String> {
    for (flag, rows) in [("--role", roles), ("--role-provider", providers)] {
        let mut seen = BTreeSet::new();
        if let Some((name, _)) = rows.iter().find(|(name, _)| !seen.insert(name)) {
            return Err(format!("{flag} '{name}' is given twice"));
        }
    }
    if let Some((name, _)) = providers
        .iter()
        .find(|(name, _)| roles.iter().all(|(role, _)| role != name))
    {
        return Err(format!(
            "--role-provider names '{name}', but no --role binds it"
        ));
    }
    roles
        .iter()
        .map(|(name, model)| {
            let provider = providers
                .iter()
                .find(|(role, _)| role == name)
                .map(|(_, account)| account.as_str());
            let remedy = format!("pass --role-provider {name}=ACCOUNT to say which");
            named(provider, model, available, &mut listing, &remedy)
                .map(|pair| (name.clone(), pair))
                .map_err(|why| format!("role '{name}': {why}"))
        })
        .collect()
}

impl Roles {
    /// Mint each pair on `bureau` with the launch's `tuning` and `max_tokens`.
    ///
    /// # Errors
    /// As [`Bureau::build`], naming the role.
    pub(crate) fn mint(
        pairs: Vec<(String, Pair)>,
        bureau: &Bureau,
        tuning: &Tuning,
        max_tokens: Option<u32>,
    ) -> Result<Self, String> {
        pairs
            .into_iter()
            .map(|(name, Pair { account, model })| {
                bureau
                    .build(&account, model, tuning, None, max_tokens)
                    .map(|provider| (name.clone(), provider))
                    .map_err(|why| format!("role '{name}': {why}"))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    /// The provider bound to `name`.
    ///
    /// # Errors
    /// If no role is configured, or none is called `name`.
    pub(crate) fn get(&self, name: &str) -> Result<Arc<Provider>, String> {
        self.0.get(name).cloned().ok_or_else(|| {
            if self.is_empty() {
                "no roles are configured, so every child runs on your own model: pass model: \
                 `inherit. Only the human can add a role, by launching exarch with --role NAME=MODEL"
                    .into()
            } else {
                let names: Vec<_> = self.0.keys().map(String::as_str).collect();
                format!(
                    "no role '{name}': the configured roles are {}; or pass model: `inherit",
                    names.join(", ")
                )
            }
        })
    }

    /// The prompt section teaching the roles: the guide, then one line per role.
    pub(crate) fn prompt(&self) -> String {
        let lines = self
            .0
            .iter()
            .map(|(name, provider)| format!("- {name}: {}", provider.model()))
            .collect::<Vec<_>>();
        format!(
            "{}\n\n{}",
            include_str!("../../data/roles.md").trim(),
            lines.join("\n")
        )
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists(models: &[&str]) -> impl FnMut(&Account) -> Result<Vec<Listed>, ListFailure> {
        let listed: Vec<Listed> = models.iter().map(|m| Listed::bare(*m)).collect();
        move |_| Ok(listed.clone())
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }

    #[test]
    fn a_binding_splits_at_the_first_equals() {
        assert_eq!(binding("r=m"), Ok(pair("r", "m")));
        assert_eq!(binding("r=a/b:c=d"), Ok(pair("r", "a/b:c=d")));
        for bad in ["", "r", "=m", "r="] {
            assert_eq!(
                binding(bad),
                Err(format!("expected NAME=VALUE, got '{bad}'"))
            );
        }
    }

    #[test]
    fn a_name_given_twice_is_refused() {
        let available = [Account::built_in("anthropic")];
        let twice = [pair("r", "m"), pair("r", "n")];
        let err = bindings(&twice, &[], &available, lists(&["m", "n"]))
            .err()
            .unwrap();
        assert_eq!(err, "--role 'r' is given twice");
        let err = bindings(
            &[pair("r", "m")],
            &[pair("r", "anthropic"), pair("r", "x")],
            &available,
            lists(&["m"]),
        )
        .err()
        .unwrap();
        assert_eq!(err, "--role-provider 'r' is given twice");
    }

    #[test]
    fn a_role_provider_without_a_role_is_refused() {
        let available = [Account::built_in("anthropic")];
        let err = bindings(&[], &[pair("x", "anthropic")], &available, lists(&[]))
            .err()
            .unwrap();
        assert_eq!(err, "--role-provider names 'x', but no --role binds it");
    }

    #[test]
    fn an_ambiguous_model_is_broken_by_its_role_provider() {
        let available = [
            Account::built_in("anthropic"),
            Account::built_in("deepseek"),
        ];
        let err = bindings(&[pair("r", "m")], &[], &available, lists(&["m"]))
            .err()
            .unwrap();
        assert!(err.starts_with("role 'r': "), "{err}");
        assert!(err.contains("--role-provider r=ACCOUNT"), "{err}");
        let tied = bindings(
            &[pair("r", "m")],
            &[pair("r", "deepseek")],
            &available,
            lists(&["m"]),
        )
        .unwrap();
        assert_eq!(tied[0].1.account, Account::built_in("deepseek"));
    }

    #[test]
    fn get_says_what_is_missing() {
        let provider = Arc::new(Provider::scripted(
            "m",
            crate::provider::scripted::Script::default(),
        ));
        let err = Roles::default().get("r").err().unwrap();
        assert!(
            err.starts_with("no roles are configured") && err.contains("--role NAME=MODEL"),
            "{err}"
        );
        let roles = Roles(BTreeMap::from([
            ("a".into(), Arc::clone(&provider)),
            ("b".into(), Arc::clone(&provider)),
        ]));
        assert_eq!(
            roles.get("r").err().unwrap(),
            "no role 'r': the configured roles are a, b; or pass model: `inherit"
        );
        assert!(Arc::ptr_eq(&roles.get("a").unwrap(), &provider));
    }
}
