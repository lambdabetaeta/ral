//! The models the human bound to names at launch for spawned agents.

use crate::provider::models::{ListFailure, Listed};
use crate::provider::state::named;
use crate::provider::{Account, Bureau, Choice, Provider, Tuning, Via};
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

/// Split `NAME=TAGS` as [`binding`] does, the tags as [`Via::parse`] takes
/// them; a clap value parser.
///
/// # Errors
/// As [`binding`], or as [`Via::parse`] naming the role.
pub fn via_binding(s: &str) -> Result<(String, Via), String> {
    let (name, tags) = binding(s)?;
    let via = Via::parse(&tags).map_err(|why| format!("role '{name}': {why}"))?;
    Ok((name, via))
}

/// The value `rows` gives `name`.
fn lookup<'a, T>(rows: &'a [(String, T)], name: &str) -> Option<&'a T> {
    rows.iter()
        .find(|(row, _)| row == name)
        .map(|(_, value)| value)
}

/// Refuse a name `flag` gives twice.
fn once_each<T>(flag: &str, rows: &[(String, T)]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    match rows.iter().find(|(name, _)| !seen.insert(name)) {
        Some((name, _)) => Err(format!("{flag} '{name}' is given twice")),
        None => Ok(()),
    }
}

/// Refuse a name `flag` gives that no `--role` in `roles` binds.
fn bound<T>(flag: &str, rows: &[(String, T)], roles: &[(String, String)]) -> Result<(), String> {
    match rows.iter().find(|(name, _)| lookup(roles, name).is_none()) {
        Some((name, _)) => Err(format!("{flag} names '{name}', but no --role binds it")),
        None => Ok(()),
    }
}

/// Each `--role` resolved to its choice: its model, on its `--role-provider`
/// account when one is given, via its `--role-via` upstreams when those are.
///
/// # Errors
/// If a role, a `--role-provider` or a `--role-via` is given twice, if a
/// `--role-provider` or a `--role-via` names no role, or if a role's model
/// resolves to no single account.
pub(crate) fn bindings(
    roles: &[(String, String)],
    providers: &[(String, String)],
    vias: &[(String, Via)],
    available: &[Account],
    mut listing: impl FnMut(&Account) -> Result<Vec<Listed>, ListFailure>,
) -> Result<Vec<(String, Choice)>, String> {
    once_each("--role", roles)?;
    once_each("--role-provider", providers)?;
    once_each("--role-via", vias)?;
    bound("--role-provider", providers, roles)?;
    bound("--role-via", vias, roles)?;
    roles
        .iter()
        .map(|(name, model)| {
            let provider = lookup(providers, name).map(String::as_str);
            let remedy = format!("pass --role-provider {name}=ACCOUNT to say which");
            named(
                provider,
                model,
                lookup(vias, name),
                available,
                &mut listing,
                &remedy,
            )
            .map(|choice| (name.clone(), choice))
            .map_err(|why| format!("role '{name}': {why}"))
        })
        .collect()
}

impl Roles {
    /// Mint each choice on `bureau` with the launch's `tuning` and
    /// `max_tokens`.
    ///
    /// # Errors
    /// As [`Bureau::build`], naming the role.
    pub(crate) fn mint(
        choices: Vec<(String, Choice)>,
        bureau: &Bureau,
        tuning: &Tuning,
        max_tokens: Option<u32>,
    ) -> Result<Self, String> {
        choices
            .into_iter()
            .map(|(name, choice)| {
                bureau
                    .build(choice, tuning, max_tokens)
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

    fn via(name: &str, tags: &str) -> (String, Via) {
        (name.into(), Via::parse(tags).unwrap())
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
        let err = bindings(&twice, &[], &[], &available, lists(&["m", "n"]))
            .err()
            .unwrap();
        assert_eq!(err, "--role 'r' is given twice");
        let err = bindings(
            &[pair("r", "m")],
            &[pair("r", "anthropic"), pair("r", "x")],
            &[],
            &available,
            lists(&["m"]),
        )
        .err()
        .unwrap();
        assert_eq!(err, "--role-provider 'r' is given twice");
        let err = bindings(
            &[pair("r", "m")],
            &[],
            &[via("r", "a"), via("r", "b")],
            &available,
            lists(&["m"]),
        )
        .err()
        .unwrap();
        assert_eq!(err, "--role-via 'r' is given twice");
    }

    #[test]
    fn a_role_provider_without_a_role_is_refused() {
        let available = [Account::built_in("anthropic")];
        let err = bindings(&[], &[pair("x", "anthropic")], &[], &available, lists(&[]))
            .err()
            .unwrap();
        assert_eq!(err, "--role-provider names 'x', but no --role binds it");
    }

    #[test]
    fn a_role_via_without_a_role_is_refused() {
        let available = [Account::built_in("openrouter")];
        let err = bindings(&[], &[], &[via("x", "a")], &available, lists(&[]))
            .err()
            .unwrap();
        assert_eq!(err, "--role-via names 'x', but no --role binds it");
    }

    #[test]
    fn a_roles_via_rides_its_choice() {
        let available = [Account::built_in("openrouter")];
        let bound = bindings(
            &[pair("r", "m"), pair("s", "m")],
            &[],
            &[via("r", "deepinfra/fp8,novita")],
            &available,
            lists(&["m"]),
        )
        .unwrap();
        assert_eq!(bound[0].1.via, Some(via("r", "deepinfra/fp8,novita").1));
        assert_eq!(bound[1].1.via, None);
    }

    #[test]
    fn an_ambiguous_model_is_broken_by_its_role_provider() {
        let available = [
            Account::built_in("anthropic"),
            Account::built_in("deepseek"),
        ];
        let err = bindings(&[pair("r", "m")], &[], &[], &available, lists(&["m"]))
            .err()
            .unwrap();
        assert!(err.starts_with("role 'r': "), "{err}");
        assert!(err.contains("--role-provider r=ACCOUNT"), "{err}");
        let tied = bindings(
            &[pair("r", "m")],
            &[pair("r", "deepseek")],
            &[],
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
