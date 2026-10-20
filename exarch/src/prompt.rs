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
    sections.push((Some("Ral"), include_str!("../data/ral.md").into()));
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
/// exist to filter by, so the agent's [`BuiltinIndex::apply`] fills the slot
/// and appends each granted family's section as the agent is constructed.
///
/// [`BuiltinIndex::apply`]: crate::agent::index::BuiltinIndex::apply
pub(crate) const BUILTIN_INDEX_PLACEHOLDER: &str = "@@EXARCH_BUILTIN_INDEX@@";

/// The same stand-in for the one host fact that is not the host's: which agent
/// is reading.  A name is the handle every other agent addresses it by and the
/// only way it picks its own row out of a roster, so it cannot be baked with
/// the rest of [`host_section`] — the agent's
/// [`BuiltinIndex::apply`](crate::agent::index::BuiltinIndex::apply) fills it.
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

/// The `AGENTS.md` chain to inject, outermost first so the most specific
/// file's recency dominates: the operator's `<config>/AGENTS.md` — the trusted
/// XDG root [`crate::config`] loads `config.ral` from — then every `AGENTS.md`
/// along [`work_tree_chain`](ral_core::path::work_tree_chain), the repo root
/// down to `cwd`.  Outside a repo, only `cwd/AGENTS.md`.
///
/// The repo files sit in the agent's own writable tree, so unlike `config.ral`
/// they are untrusted — but they add prompt text, never capabilities, and
/// cannot widen the grant.
fn discover_agents(cwd: &Path, config_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let global = config_dir.join("AGENTS.md");
    if global.exists() {
        files.push(global);
    }
    for dir in ral_core::path::work_tree_chain(cwd) {
        let file = dir.join("AGENTS.md");
        if file.exists() {
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
            &std::fs::read_to_string(path)
                .map_err(|e| format!("{}: {e}", ral_core::path::shown(path)))?,
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
fn grant_summary<'a>(stack: impl IntoIterator<Item = &'a Capabilities>) -> String {
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
