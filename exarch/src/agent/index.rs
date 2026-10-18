//! Per-agent resolution of the launch's prompt: the builtin index filtered by
//! the agent's own grants, its name, and the sections those grants teach.

use crate::agent::Role;
use crate::prompt::{BUILTIN_INDEX_PLACEHOLDER, NAME_PLACEHOLDER};

/// One agent's construction-fixed authorities — the same facts the desk reads
/// when refusing, so prompt and refusal cannot disagree.
pub(crate) struct Grants<'a> {
    pub role: &'a Role,
    pub allow_schedule: bool,
    pub spawns: bool,
}

/// Each grant admits a family of verbs to the index and, where the family has
/// one, a prompt section teaching it: `(held, verbs, section)`. In section
/// order — the return contract last, where recency carries. Both filters are
/// prompt-only; the desk remains the runtime authority, this spares the model
/// a step finding out. The schedule family is still taught by the persona;
/// moving that prose here is one `Some`.
///
/// `exarch-agents` is one builtin covering two grants that do not coincide: `start`
/// needs spawn fuel, but `reply`/`read` need only a returning role — a leaf
/// agent with no fuel left still holds `reply`. So the verb name is withheld
/// only when *neither* grant holds, while the sections that teach it each
/// gate on their own grant alone.
type Section = Option<(&'static str, &'static str)>;
fn families(g: &Grants) -> [(bool, &'static [&'static str], Section); 5] {
    let returns = g.role.returns();
    [
        (g.allow_schedule, &["exarch-schedules"], None),
        (g.spawns || returns, &["exarch-agents"], None),
        (
            g.spawns,
            &[],
            Some(("Agents", include_str!("../../data/agents.md"))),
        ),
        (
            matches!(g.role, Role::Child { .. }),
            &[],
            Some(("Spawned", include_str!("../../data/spawned.md"))),
        ),
        (
            returns,
            &[],
            Some(("Reply", include_str!("../../data/reply.md"))),
        ),
    ]
}

/// Every verb the product installs, resolved once from a booted engine's
/// builtin names — the surface is fixed per product, so no later prompt
/// resolution asks an engine again. Shared by a whole fleet: a fork or desk
/// spawn applies its own grants against its parent's resolution.
pub(crate) struct BuiltinIndex {
    /// Sorted and deduped: the shell's installed builtins, the documented
    /// prelude, and the agent library — ral closures sourced from `agent.ral`,
    /// not registered builtins, hence
    /// [`agent_library_docs`](crate::library::agent_library_docs).
    /// The engine's names keep the `_`-prefixed internals, so the filter
    /// lives here and covers all three.
    names: Vec<String>,
}

impl BuiltinIndex {
    /// `builtins` is the engine's own table, as `Transport::builtin_names`
    /// reads it.
    pub(crate) fn resolve(builtins: Vec<String>) -> std::sync::Arc<Self> {
        let prelude = ral_core::builtins::help::prelude_names()
            .into_iter()
            .map(str::to_string);
        let library = crate::library::agent_library_docs()
            .into_iter()
            .map(|(name, _doc)| name);
        let mut names: Vec<String> = builtins
            .into_iter()
            .chain(prelude)
            .chain(library)
            .filter(|n| !n.starts_with('_'))
            .collect();
        names.sort_unstable();
        names.dedup();
        std::sync::Arc::new(Self { names })
    }

    /// The index section for one agent: names only, since the agent
    /// `explain`s any one for its docs on demand, minus every family whose
    /// grant is absent.
    fn section(&self, grants: &Grants) -> String {
        let families = families(grants);
        let withheld = |name: &str| {
            families
                .iter()
                .any(|(held, verbs, _)| !held && verbs.contains(&name))
        };
        let names: Vec<&str> = self
            .names
            .iter()
            .map(String::as_str)
            .filter(|n| !withheld(n))
            .collect();
        format!(
            "Every builtin and prelude function, by name: call `explain <name>` for any one's signature and docs:\n\n{}",
            names.join(", ")
        )
    }

    /// Resolve `template` for one agent: fill both placeholders — the
    /// grant-filtered index and the agent's own name — then append each held
    /// family's section. A fresh model therefore sees neither a verb nor a
    /// prompt section it is certain to be refused.
    ///
    /// The name is unconditional where the family sections are not: an agent
    /// with no fuel and no return still appears in every roster and can still
    /// be messaged, so it still needs to know which row is its own.
    pub(crate) fn apply(&self, template: &str, grants: &Grants, name: &str) -> String {
        let mut resolved = template
            .replace(BUILTIN_INDEX_PLACEHOLDER, &self.section(grants))
            .replace(NAME_PLACEHOLDER, name);
        for (held, _, section) in families(grants) {
            if let (true, Some((heading, body))) = (held, section) {
                append_section(&mut resolved, heading, body);
            }
        }
        resolved
    }
}

/// Append a late-bound prompt section while keeping the renderer's one-blank-
/// line spacing and final newline, even for a custom base supplied by a host.
fn append_section(prompt: &mut String, heading: &str, body: &str) {
    prompt.truncate(prompt.trim_end().len());
    prompt.push_str("\n\n# ");
    prompt.push_str(heading);
    prompt.push_str("\n\n");
    prompt.push_str(body.trim_end());
    prompt.push('\n');
}
