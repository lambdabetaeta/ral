//! Assemble the system prompt as `(heading, body)` sections; the shape of the
//! prompt is the shape of the Vec built in `assemble`.

pub mod host;

use crate::policy::Invocation;
use crate::skill;
use ral_core::capability::{Capabilities, ExecKey, GrantStack};
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// The stand-in system prompt for `--chat`, which assembles none.
///
/// A bare period: the Responses adapter rejects an empty system prompt
/// and Anthropic a whitespace-only one, and chat does not branch on the
/// adapter.
pub const CHAT_SYSTEM: &str = ".";

/// The editing scheme `--edit` selects: one system-prompt section, since both
/// editing builtins are registered regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EditScheme {
    /// Teach the agent to inspect text and replace an exact string.
    Replace,
    /// Teach the agent to inspect line hashes and make changes tied to the
    /// lines it inspected.
    Hash,
}

/// Build the ordered `(heading, body)` sections [`render`] walks.
///
/// Every section but Builtins and the Host grant is agent-invariant and
/// stands as baked here; those hold [`BUILTIN_INDEX_PLACEHOLDER`] and
/// [`GRANT_PLACEHOLDER`], the prompt being assembled once at boot. Each is
/// resolved as an agent is constructed, once its own bits and grants exist.
///
/// # Errors
/// If reading a `--system` file or a discovered `AGENTS.md` fails.
pub fn assemble(
    files: &[PathBuf],
    invocation: &Invocation,
    app: crate::app::App,
    cwd: &Path,
    config_dir: &Path,
    interactive: bool,
    edit: EditScheme,
) -> Result<String, String> {
    let mut sections: Vec<(Option<&str>, String)> = Vec::new();
    // Persona is the only section `--system` replaces; the rest stand.
    sections.push((
        None,
        if files.is_empty() {
            include_str!("../data/system.md").into()
        } else {
            read_files(files)?
        },
    ));
    sections.push((
        Some("Ral"),
        format!(
            "{}\n{}",
            include_str!("../data/ral.md"),
            include_str!("../data/ral-examples.md")
        ),
    ));
    sections.push((
        Some("Editing"),
        match edit {
            EditScheme::Hash => include_str!("../data/edit-hash.md").into(),
            EditScheme::Replace => include_str!("../data/edit-replace.md").into(),
        },
    ));
    sections.push((Some("Builtins"), BUILTIN_INDEX_PLACEHOLDER.to_string()));
    sections.push((Some("Tasks"), include_str!("../data/tasks.md").into()));
    sections.push((
        Some("Script style"),
        include_str!("../data/script-style.md").into(),
    ));
    sections.push((
        Some("Context management"),
        include_str!("../data/context.md").into(),
    ));
    sections.push((Some("Host"), host_section(app)));
    let agents = discover_agents(cwd, config_dir);
    if !agents.is_empty() {
        sections.push((Some("Workspace"), read_files(&agents)?));
    }
    let skills = skill::discover_metadata(cwd, config_dir, &invocation.stack);
    if !skills.is_empty() {
        sections.push((Some("Skills"), skills_section(&skills)));
    }
    if interactive {
        sections.push((Some("Surfacing"), include_str!("../data/surface.md").into()));
    }
    Ok(render(&sections))
}

/// [`assemble`]'s stand-in for the per-agent builtin index, never sent to a
/// model: the prompt is baked once at boot, before any agent's own grants
/// exist to filter by, so [`BuiltinIndex::apply`] fills the slot and appends
/// each granted family's section as the agent is constructed.
pub(crate) const BUILTIN_INDEX_PLACEHOLDER: &str = "@@EXARCH_BUILTIN_INDEX@@";

/// The same stand-in for the one host fact that is not the host's: which agent
/// is reading.  A name is the handle every other agent addresses it by and the
/// only way it picks its own row out of a roster, so it cannot be baked with
/// the rest of [`host_section`] — [`BuiltinIndex::apply`] fills it.
pub(crate) const NAME_PLACEHOLDER: &str = "@@EXARCH_AGENT_NAME@@";

/// The same stand-in for the scratch path, which is the engine's to name: the
/// trunk fills it from the booted engine's own environment.
pub const SCRATCH_PLACEHOLDER: &str = "@@EXARCH_SCRATCH_PATH@@";

/// The same stand-in for the grant, which is each agent's own: a child's
/// engine holds a layer no other agent's does, so [`with_grant`] fills it from
/// the agent's own engine as the agent is constructed.
pub const GRANT_PLACEHOLDER: &str = "@@EXARCH_GRANT@@";

/// Fill [`GRANT_PLACEHOLDER`] with the stack an agent's every eval runs
/// under: its engine session's layers, then the ceiling its host pushes.
pub(crate) fn with_grant(template: &str, session: &GrantStack, ceiling: &GrantStack) -> String {
    template.replace(
        GRANT_PLACEHOLDER,
        &grant_summary(session.iter().chain(ceiling)),
    )
}

/// One agent's construction-fixed authorities — the same bits the desk reads
/// when refusing, so prompt and refusal cannot disagree.
pub(crate) struct Grants {
    pub returns: bool,
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
/// needs spawn fuel, but `reply`/`read` need only `returns` — a leaf agent
/// with no fuel left still holds `reply`. So the verb name is withheld only
/// when *neither* grant holds, while the two prompt sections that teach it
/// each gate on their own grant alone.
type Section = Option<(&'static str, &'static str)>;
fn families(g: &Grants) -> [(bool, &'static [&'static str], Section); 4] {
    [
        (g.allow_schedule, &["exarch-schedules"], None),
        (g.spawns || g.returns, &["exarch-agents"], None),
        (
            g.spawns,
            &[],
            Some(("Agents", include_str!("../data/agents.md"))),
        ),
        (
            g.returns,
            &[],
            Some(("Reply", include_str!("../data/reply.md"))),
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

/// The `AGENTS.md` chain to inject, outermost first so the most specific
/// file's recency dominates: the operator's `<config>/AGENTS.md` — the trusted
/// XDG root [`crate::config`] loads `config.ral` from — then every `AGENTS.md`
/// from the repo root down to `cwd`, the walk stopping at the first ancestor
/// holding a `.git` entry (file or directory, so worktrees and submodules
/// count).  Outside a repo, only `cwd/AGENTS.md`.
///
/// The repo files sit in the agent's own writable tree, so unlike `config.ral`
/// they are untrusted — but they add prompt text, never capabilities, and
/// cannot widen the grant.
fn discover_agents(cwd: &Path, config_dir: &Path) -> Vec<PathBuf> {
    let repo_root = ral_core::path::find_git_entry(cwd)
        .and_then(|dot_git| dot_git.parent().map(Path::to_path_buf));
    let mut scan: Vec<PathBuf> = match repo_root {
        Some(root) => {
            let mut dirs: Vec<PathBuf> = cwd
                .ancestors()
                .take_while(|dir| *dir != root)
                .map(Path::to_path_buf)
                .collect();
            dirs.push(root);
            dirs
        }
        None => vec![cwd.to_path_buf()],
    };
    scan.reverse();

    let mut files = Vec::new();
    let global = config_dir.join("AGENTS.md");
    if ral_core::path::exists(&global.to_string_lossy()) {
        files.push(global);
    }
    for dir in scan {
        let file = dir.join("AGENTS.md");
        if ral_core::path::exists(&file.to_string_lossy()) {
            files.push(file);
        }
    }
    files
}

/// Concatenate files into one section body, blank-line separated; the caller
/// fixes the order.  Serves both `--system FILE...` and [`discover_agents`].
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:system-prompt-files] reads the --system prompt files and the discovered AGENTS.md chain (the repo/cwd ones untrusted, from the agent's own tree) into the system prompt at load time; not turn-time I/O"
)]
fn read_files(files: &[PathBuf]) -> Result<String, String> {
    let mut buf = String::new();
    for path in files {
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(
            &std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
        );
    }
    Ok(buf)
}

/// Join the sections one blank line apart, headed ones under a `# heading`.
///
/// Bodies are `trim_end`ed first so that gap stays exactly one line however
/// many blank lines a section's own source happened to end with.
pub fn render(sections: &[(Option<&str>, String)]) -> String {
    sections
        .iter()
        .map(|(h, body)| match h {
            None => body.trim_end().to_string(),
            Some(h) => format!("# {h}\n\n{}", body.trim_end()),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n"
}

/// The `Host` section: where the agent stands and when "now" is, then the
/// authority it holds.
///
/// Every line is a *host* truth, which is why the composition is exarch's
/// alone — synod's engine lives in a guest VM where none of them hold, so it
/// builds its own around the shared [`GRANT_PLACEHOLDER`].
pub fn host_section(app: crate::app::App) -> String {
    let state = app.xdg_dir(ral_core::path::XdgKind::State);
    format!(
        "You are an agent named '{NAME_PLACEHOLDER}'.\n\n{}\n{GRANT_PLACEHOLDER}- scratch: `${}` = {SCRATCH_PLACEHOLDER}\n",
        host::snapshot(&state),
        app.scratch_var()
    )
}

/// Whether a layer is worth summarising: it restricts, or its exec names a run
/// place.
///
/// Under a nested fs restriction such a place stays readable, which the reader
/// needs to know even of a layer that denotes `t`.
fn says_something(caps: &Capabilities) -> bool {
    caps.is_restrictive()
        || caps
            .exec
            .as_ref()
            .is_some_and(|grant| grant.iter().any(|(_, e)| e.positive))
}

/// The grant a stack denotes, as the model reads it.
///
/// A static legend teaching the notation and the runtime denial string, then
/// each distinct layer that [`says_something`], oldest first, then the net
/// line once.  The stack is the meet, so describing each layer describes the
/// whole ceiling, and a repeat adds nothing.
///
/// `None` is "unrestricted" (no attenuation at this layer) and an empty
/// container "(none)".  The caller appends the `- scratch:` bullet, since
/// exarch names a seeded env var and host path where synod names the guest's
/// tmpfs.
pub fn grant_summary<'a>(stack: impl IntoIterator<Item = &'a Capabilities>) -> String {
    let mut layers: Vec<&Capabilities> = Vec::new();
    for caps in stack {
        if says_something(caps) && !layers.contains(&caps) {
            layers.push(caps);
        }
    }
    // The denial legend would describe a runtime event that cannot occur.
    if layers.is_empty() {
        return "Ambient authority: every command, path, and network call is permitted; \
the sandbox is the trust boundary.\n\n"
            .into();
    }
    let mut s = legend();
    if layers.len() > 1 {
        let _ = writeln!(
            s,
            "The grant is the meet of {} layers, oldest first.",
            layers.len()
        );
    }
    for (i, caps) in layers.iter().enumerate() {
        if layers.len() > 1 {
            let _ = writeln!(s, "\nLayer {}:", i + 1);
        }
        s.push_str(&layer_lines(caps));
    }
    let denied = layers.iter().any(|c| c.net.is_some());
    let _ = writeln!(s, "\n- net: {}", if denied { "deny" } else { "inherit" });
    s
}

fn legend() -> String {
    format!("{}\n\n", include_str!("../data/grant-legend.md").trim_end())
}

/// One layer's bullets bar net and scratch.
fn layer_lines(caps: &Capabilities) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "- exec: {}", exec_line(caps));
    let dirs = exec_dirs_line(caps);
    if !dirs.is_empty() {
        let _ = writeln!(s, "- exec dirs: {dirs}");
    }
    let denies = exec_denies(caps);
    if !denies.is_empty() {
        let _ = writeln!(s, "- exec deny: {}", denies.join(", "));
    }
    if let Some(fs) = &caps.fs {
        let _ = writeln!(s, "- fs read: {}", or_none(&fs.read_prefixes));
        let _ = writeln!(s, "- fs write: {}", or_none(&fs.write_prefixes));
        if !fs.deny_paths.is_empty() {
            let _ = writeln!(s, "- fs deny: {}", join_str(&fs.deny_paths));
        }
    }
    if let Some(verbs) = &caps.editor {
        let _ = writeln!(s, "- editor: {}", verbs_line(verbs));
    }
    if let Some(verbs) = &caps.shell {
        let _ = writeln!(s, "- shell: {}", verbs_line(verbs));
    }
    s
}

/// Admitted commands, comma-joined: bare names, then path keys as their
/// author spelled them.  Directory allowances are surfaced by
/// [`exec_dirs_line`], exclusions by [`exec_denies`], so only the other
/// allowed keys land here.  Beside `every` they add no running, and are
/// listed as authored.
fn exec_line(caps: &Capabilities) -> String {
    let Some(grant) = &caps.exec else {
        return "unrestricted".into();
    };
    let admitted: Vec<String> = (grant.iter())
        .filter(|(key, e)| !matches!(key, ExecKey::Dir(_)) && e.positive)
        .map(|(key, _)| key.to_string())
        .collect();
    match (grant.every(), admitted.is_empty()) {
        (true, true) => "every program, less the exclusions below".into(),
        (true, false) => format!(
            "every program, less the exclusions below; also named: {}",
            admitted.join(", ")
        ),
        (false, true) => "(none)".into(),
        (false, false) => admitted.join(", "),
    }
}

/// Allowed directories, each rendered as its `dir` target.  Empty when
/// nothing allows by directory.
fn exec_dirs_line(caps: &Capabilities) -> String {
    caps.exec.as_ref().map_or_else(String::new, |grant| {
        (grant.iter())
            .filter(|(key, e)| matches!(key, ExecKey::Dir(_)) && e.positive)
            .map(|(key, _)| key.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// Names, path keys and directory prefixes carrying an exclusion, which wins
/// over every allowance, a covering directory's included.  A key allowed and
/// excluded appears here and in the allowed lists.
fn exec_denies(caps: &Capabilities) -> Vec<String> {
    caps.exec.as_ref().map_or_else(Vec::new, |grant| {
        (grant.iter())
            .filter(|(_, e)| e.negative)
            .map(|(key, _)| key.to_string())
            .collect()
    })
}

fn verbs_line<V: Copy>(verbs: &BTreeSet<V>) -> String
where
    &'static str: From<V>,
{
    let names: Vec<&str> = verbs.iter().map(|&v| v.into()).collect();
    if names.is_empty() {
        "(none)".into()
    } else {
        names.join(", ")
    }
}

fn or_none<S: AsRef<str>>(v: &[S]) -> String {
    if v.is_empty() {
        "(none)".into()
    } else {
        join_str(v)
    }
}

/// Comma-join any slice whose items borrow as `&str` — the prefix lists hold
/// `FrozenPath`es, not strings.
fn join_str<S: AsRef<str>>(v: &[S]) -> String {
    v.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(", ")
}

/// The Skills section: `name: description` per skill, the same
/// progressive-disclosure shape as [`BuiltinIndex`].
fn skills_section(skills: &[skill::Skill]) -> String {
    let mut body =
        String::from("Available skills (call `skill <name>` to load, `skill-list` to refresh):\n");
    for s in skills {
        let _ = writeln!(body, "- {}: {}", s.name, s.description);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The name set the index lists, split out of its preamble so a test
    /// asserts membership without depending on order.
    fn names(index: &str) -> HashSet<&str> {
        let list = index.split("\n\n").nth(1).expect("index has a name list");
        list.split(", ").collect()
    }

    fn index_of_boot() -> std::sync::Arc<BuiltinIndex> {
        let shell = crate::boot::test_shell();
        BuiltinIndex::resolve(shell.builtin_names().map(str::to_string).collect())
    }

    fn index_for(grants: &Grants) -> String {
        index_of_boot().section(grants)
    }

    #[test]
    fn builtin_index_lists_agents_and_schedule_family_when_both_bits_hold() {
        let index = index_for(&Grants {
            returns: true,
            allow_schedule: true,
            spawns: true,
        });
        let n = names(&index);
        assert!(n.contains("exarch-agents"));
        assert!(n.contains("exarch-schedules"));
    }

    /// Zero spawn fuel narrows what `exarch-agents `start` can do, never whether the
    /// name is offered at all: a fuelless returning agent still needs
    /// `exarch-agents `reply` to hand back its value.
    #[test]
    fn builtin_index_holds_agents_verb_for_a_fuelless_returning_agent() {
        let index = index_for(&Grants {
            returns: true,
            allow_schedule: true,
            spawns: false,
        });
        let n = names(&index);
        assert!(
            n.contains("exarch-agents"),
            "a returning agent still holds `exarch-agents `reply` with no spawn fuel left"
        );
    }

    /// Non-returning, no fuel: the interactive trunk and every `/branch`
    /// child, once run to zero depth.
    #[test]
    fn builtin_index_omits_agents_verb_when_neither_spawns_nor_returns() {
        let index = index_for(&Grants {
            returns: false,
            allow_schedule: true,
            spawns: false,
        });
        let n = names(&index);
        assert!(
            !n.contains("exarch-agents"),
            "must not advertise a verb the desk always refuses"
        );
        assert!(
            n.contains("exarch-schedules"),
            "a granted agent still holds the schedule family"
        );
    }

    #[test]
    fn builtin_index_omits_schedule_family_for_an_ungranted_agent() {
        let index = index_for(&Grants {
            returns: true,
            allow_schedule: false,
            spawns: true,
        });
        let n = names(&index);
        assert!(!n.contains("exarch-schedules"));
        assert!(
            n.contains("exarch-agents"),
            "a returning agent still holds `exarch-agents`"
        );
    }

    /// The two family sections gate independently: `Agents` on spawn fuel,
    /// `Reply` on `returns` — a fuelless returning agent gets the latter and
    /// not the former.
    #[test]
    fn builtin_index_apply_gates_agents_and_reply_sections_independently() {
        let index = index_of_boot();
        let resolved = index.apply(
            BUILTIN_INDEX_PLACEHOLDER,
            &Grants {
                returns: true,
                allow_schedule: false,
                spawns: false,
            },
            "leaf",
        );
        assert!(!resolved.contains("# Agents"));
        assert!(resolved.contains("# Reply"));
    }

    #[test]
    fn builtin_index_apply_substitutes_the_placeholder() {
        let index = index_of_boot();
        let grants = Grants {
            returns: false,
            allow_schedule: true,
            spawns: false,
        };
        let template = format!("before\n\n{BUILTIN_INDEX_PLACEHOLDER}\n\nafter");
        let resolved = index.apply(&template, &grants, "chunk-3");
        assert_eq!(
            resolved,
            format!("before\n\n{}\n\nafter", index.section(&grants))
        );
        assert!(!resolved.contains(BUILTIN_INDEX_PLACEHOLDER));
    }

    /// A custom base need not carry either placeholder.
    #[test]
    fn builtin_index_apply_is_a_noop_without_the_placeholder() {
        assert_eq!(
            index_of_boot().apply(
                "plain text",
                &Grants {
                    returns: false,
                    allow_schedule: false,
                    spawns: false,
                },
                "chunk-3",
            ),
            "plain text"
        );
    }

    /// The name reaches the agent through `Host`, whoever it is — an agent with
    /// neither fuel nor a return still appears in a roster and can still be
    /// messaged, so it still needs to know which row is its own.
    #[test]
    fn apply_names_the_agent_to_itself() {
        let index = index_of_boot();
        let template = format!("before\n\n{NAME_PLACEHOLDER}\n\nafter");
        let resolved = index.apply(
            &template,
            &Grants {
                returns: false,
                allow_schedule: false,
                spawns: false,
            },
            "chunk-3",
        );
        assert!(
            resolved.contains("chunk-3") && !resolved.contains(NAME_PLACEHOLDER),
            "the placeholder must never reach a model: {resolved}"
        );
    }

    /// A layer withholding only `detach` is no ambient authority: its verbs
    /// are listed.
    #[test]
    fn a_layer_withholding_only_detach_lists_its_verbs() {
        use ral_core::capability::ShellVerb;
        let layer = Capabilities {
            shell: Some([ShellVerb::Chdir].into()),
            ..Capabilities::root()
        };
        let text = grant_summary(&GrantStack::of(layer));
        assert!(!text.contains("Ambient authority"), "{text}");
        assert!(text.contains("- shell: chdir\n"), "{text}");
    }

    /// A child's session already holds its parent's ceiling, which the host
    /// pushes again on every run: the meet is idempotent, so the repeat is
    /// told once.
    #[test]
    fn a_repeated_layer_is_told_once() {
        use ral_core::capability::Net;
        let ceiling = GrantStack::of(Capabilities {
            net: Some(Net::Offline),
            ..Capabilities::root()
        });
        let text = with_grant(GRANT_PLACEHOLDER, &ceiling, &ceiling);
        assert!(!text.contains("meet of"), "{text}");
        assert!(text.contains("- net: deny\n"), "{text}");
    }

    /// The Host section leaves the grant to each agent, and once filled, the
    /// scratch bullet stands on its own line whether the grant is ambient or
    /// restricted.
    #[test]
    fn the_scratch_bullet_survives_any_grant() {
        use ral_core::capability::Net;
        let template = host_section(crate::app::EXARCH);
        let ambient = GrantStack::of(Capabilities::root());
        let offline = GrantStack::of(Capabilities {
            net: Some(Net::Offline),
            ..Capabilities::root()
        });
        for stack in [ambient, offline] {
            let text = with_grant(&template, &stack, &stack);
            assert!(
                text.lines()
                    .any(|l| l.starts_with("- scratch:") && l.contains(SCRATCH_PLACEHOLDER)),
                "{text}"
            );
        }
    }
}
