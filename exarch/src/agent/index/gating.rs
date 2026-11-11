//! Which verbs and sections each grant admits to one agent's prompt.

use super::*;
use crate::provider::Provider;
use crate::provider::scripted::Script;
use std::collections::BTreeMap;
use std::sync::Arc;

const PLACEHOLDER_PREFIX: &str = "@@EXARCH_";
const TAUGHT: [&str; 4] = ["Agents", "Roles", "Spawned", "Reply"];

/// `(spawned, returns, allow_schedule, spawns)` as a [`Grants`].
fn grants(held: [bool; 4], roles: &Roles) -> Grants<'_> {
    let [spawned, returns, allow_schedule, spawns] = held;
    Grants {
        spawned,
        returns,
        allow_schedule,
        spawns,
        roles,
    }
}

fn index() -> Arc<BuiltinIndex> {
    BuiltinIndex::resolve(vec![
        "exarch-schedules".into(),
        "exarch-agents".into(),
        "_internal".into(),
        "explainable".into(),
    ])
}

/// The names the index section lists for `grants`.
fn listed(index: &BuiltinIndex, grants: &Grants) -> Vec<String> {
    let section = index.section(grants);
    let (_, names) = section.rsplit_once("\n\n").expect("a heading, then names");
    names.split(", ").map(str::to_string).collect()
}

/// The taught sections `prompt` carries, in order.
fn taught(prompt: &str) -> Vec<&'static str> {
    prompt
        .lines()
        .filter_map(|line| {
            let heading = line.strip_prefix("# ")?;
            TAUGHT.into_iter().find(|taught| *taught == heading)
        })
        .collect()
}

#[test]
fn the_schedule_verb_is_listed_only_when_scheduling_is_allowed() {
    let (index, roles) = (index(), Roles::default());
    for allowed in [false, true] {
        let names = listed(&index, &grants([false, false, allowed, false], &roles));
        assert_eq!(
            names.contains(&"exarch-schedules".to_string()),
            allowed,
            "allow_schedule = {allowed}"
        );
    }
}

/// `start` needs fuel and `reply`/`read` a return: the one verb stays while
/// either holds.
#[test]
fn the_agents_verb_is_withheld_only_when_neither_spawning_nor_returning() {
    let (index, roles) = (index(), Roles::default());
    for (spawns, returns) in [(false, false), (true, false), (false, true), (true, true)] {
        let names = listed(&index, &grants([false, returns, false, spawns], &roles));
        assert_eq!(
            names.contains(&"exarch-agents".to_string()),
            spawns || returns,
            "spawns = {spawns}, returns = {returns}"
        );
    }
}

#[test]
fn each_section_gates_on_its_own_grant_and_the_return_contract_comes_last() {
    let provider = Arc::new(Provider::scripted("small-model", Script::default()));
    let some = Roles(BTreeMap::from([("cheap".into(), provider)]));
    let none = Roles::default();
    let index = index();
    let taught_under = |held, roles| taught(&index.apply("", &grants(held, roles), "x"));
    assert_eq!(taught_under([false; 4], &some), [] as [&str; 0]);
    assert_eq!(
        taught_under([false, false, true, false], &some),
        [] as [&str; 0]
    );
    assert_eq!(taught_under([false, false, false, true], &none), ["Agents"]);
    assert_eq!(
        taught_under([true, false, false, false], &none),
        ["Spawned"]
    );
    // A leaf agent with no fuel left still holds the return contract.
    assert_eq!(taught_under([false, true, false, false], &none), ["Reply"]);
    assert_eq!(
        taught_under([true, true, true, true], &some),
        TAUGHT,
        "every grant held: each section once, in order"
    );
}

#[test]
fn the_agents_own_name_fills_its_slot_whatever_it_is_granted() {
    let (index, roles) = (index(), Roles::default());
    let template = format!(
        "{} then {}",
        crate::prompt::NAME_PLACEHOLDER,
        crate::prompt::BUILTIN_INDEX_PLACEHOLDER
    );
    for held in [[false; 4], [true; 4]] {
        let prompt = index.apply(&template, &grants(held, &roles), "zed");
        assert!(prompt.starts_with("zed then "), "{prompt}");
        assert!(!prompt.contains(PLACEHOLDER_PREFIX), "{prompt}");
    }
}
