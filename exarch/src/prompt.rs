//! Assemble the system prompt as `(heading, body)` sections; the shape of the
//! prompt is the shape of the Vec built in `assemble`.

pub mod host;

use crate::cli::EditScheme;
use crate::shell_eval::skill;
use ral_core::Shell;
use ral_core::types::Capabilities;
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// The stand-in system prompt for `--chat`, which assembles none.
///
/// A bare period: the Codex/Responses adapter rejects an empty system prompt
/// and Anthropic a whitespace-only one, and chat does not branch on the
/// adapter.
pub const CHAT_SYSTEM: &str = ".";

/// Build the ordered `(heading, body)` sections [`render`] walks.
///
/// Every section but Builtins is agent-invariant and stands as baked here;
/// Builtins holds only [`BUILTIN_INDEX_PLACEHOLDER`], the prompt being
/// assembled once at boot. The closing Agent/Surfacing section goes last, for
/// its recency.
///
/// # Errors
/// If reading a `--system` file or a discovered `AGENTS.md` fails.
pub fn assemble(
    files: &[PathBuf],
    caps: &Capabilities,
    scratch: &crate::bootstrap::Scratch,
    cwd: &Path,
    config_dir: &Path,
    headless: bool,
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
        Some("Script style"),
        include_str!("../data/script-style.md").into(),
    ));
    sections.push((Some("Host"), host_section(caps, scratch)));
    let agents = discover_agents(cwd, config_dir);
    if !agents.is_empty() {
        sections.push((Some("Workspace"), read_files(&agents)?));
    }
    let skills = skill::discover_metadata(cwd, config_dir, caps);
    if !skills.is_empty() {
        sections.push((Some("Skills"), skills_section(&skills)));
    }
    if headless {
        sections.push((Some("Agent"), include_str!("../data/agent.md").into()));
    } else {
        sections.push((Some("Surfacing"), include_str!("../data/surface.md").into()));
    }
    Ok(render(&sections))
}

/// [`assemble`]'s stand-in for the per-agent builtin index, never sent to a
/// model: the prompt is baked once at boot, before any agent's own
/// `returns`/`allow_schedule` bits exist to filter by, so
/// [`BuiltinIndexes::apply`] fills the slot as each agent is constructed.
pub(crate) const BUILTIN_INDEX_PLACEHOLDER: &str = "@@EXARCH_BUILTIN_INDEX@@";

/// Every verb *this* agent can name, comma-joined: the shell's installed
/// builtins, the documented prelude, and the agent library — ral closures
/// sourced from `agent.ral`, not registered builtins, hence
/// [`agent_library_docs`](crate::shell_eval::builtins::agent_library_docs).
/// [`Shell::builtin_names`] keeps the `_`-prefixed internals, so the filter
/// lives here and covers all three.
///
/// Names only, since the agent `explain`s any one for its docs on demand.
/// Dropping `reply` when `!returns` and the self-wakeup family when
/// `!allow_schedule` is prompt-only — the desk refuses them regardless; this
/// spares the agent a step finding out.
fn builtin_index(shell: &Shell, returns: bool, allow_schedule: bool) -> String {
    let prelude = ral_core::builtins::help::prelude_names()
        .into_iter()
        .map(str::to_string);
    let library = crate::shell_eval::builtins::agent_library_docs()
        .into_iter()
        .map(|(name, _doc)| name);
    let mut names: Vec<String> = shell
        .builtin_names()
        .map(str::to_string)
        .chain(prelude)
        .chain(library)
        .filter(|n| !n.starts_with('_'))
        .filter(|n| returns || n != "reply")
        .filter(|n| {
            allow_schedule || !matches!(n.as_str(), "schedule" | "schedules" | "unschedule")
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    format!(
        "Every builtin and prelude function, by name — call `explain <name>` for any one's signature and docs:\n\n{}",
        names.join(", ")
    )
}

/// The four renderings of the builtin index (`returns` × `allow_schedule`),
/// resolved once from a booted shell — the installed surface is fixed per
/// product, so no later prompt resolution needs a live `Shell` again, on
/// either side of the host seam.  Shared by a whole fleet: a fork or desk
/// spawn applies its own bits against the table its parent resolved from.
pub(crate) struct BuiltinIndexes {
    /// Indexed by `returns as usize | (allow_schedule as usize) << 1`.
    by_bits: [String; 4],
}

impl BuiltinIndexes {
    pub(crate) fn resolve(shell: &Shell) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            by_bits: [
                builtin_index(shell, false, false),
                builtin_index(shell, true, false),
                builtin_index(shell, false, true),
                builtin_index(shell, true, true),
            ],
        })
    }

    /// Fill [`BUILTIN_INDEX_PLACEHOLDER`] for these bits — the same
    /// construction-fixed bits the desk reads when refusing, so a fresh model
    /// never sees a verb it is certain to be refused.  A no-op on text that
    /// never held the placeholder, so callers need not check.
    pub(crate) fn apply(&self, template: &str, returns: bool, allow_schedule: bool) -> String {
        let slot = usize::from(returns) | (usize::from(allow_schedule) << 1);
        template.replace(BUILTIN_INDEX_PLACEHOLDER, &self.by_bits[slot])
    }
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
    reason = "[io-door:silent:system-prompt-files] reads the --system prompt files and the discovered AGENTS.md chain (the repo/cwd ones untrusted, from the agent's own tree) into the system prompt at load time; not a turn-time door"
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
/// builds its own around the shared [`grant_summary`].
pub fn host_section(caps: &Capabilities, scratch: &crate::bootstrap::Scratch) -> String {
    let state = scratch
        .app()
        .xdg_dir(ral_core::path::basedir::XdgKind::State);
    let scratch_line = format!("`${}` = {}", scratch.var(), scratch.path().display());
    format!(
        "{}\n{}",
        host::snapshot(&state),
        grant_summary(caps, &scratch_line)
    )
}
/// The live grant: a static legend teaching the notation and the runtime
/// denial string, then one effect per line.
///
/// `None` is "unrestricted" — no attenuation at this layer — and an empty
/// container "(none)".
///
/// `scratch_line` is the right-hand side of the `- scratch:` bullet, left to
/// the caller because exarch names a seeded env var and host path where synod
/// names the guest's tmpfs.  Public for exactly that second caller.
pub fn grant_summary(caps: &Capabilities, scratch_line: &str) -> String {
    // Nothing attenuated (the `dangerous` profile): the denial legend would
    // describe a runtime event that cannot occur, so collapse to one line
    // plus the scratch path the agent still needs.
    let ambient = caps.exec.is_none() && caps.fs.is_none() && caps.net != Some(false);
    if ambient {
        return format!(
            "Ambient authority: every command, path, and network call is permitted; \
the sandbox is the trust boundary.\n\n- scratch: {scratch_line}\n"
        );
    }
    let mut s = String::from(include_str!("../data/grant-legend.md").trim_end());
    s.push_str("\n\n");
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
    let _ = writeln!(
        s,
        "- net: {}",
        match caps.net {
            None => "inherit",
            Some(true) => "allow",
            Some(false) => "deny",
        }
    );
    let _ = writeln!(s, "- scratch: {scratch_line}");
    s
}

/// Per-command exec policy as `name` or `name[sub1,sub2,...]`, comma-joined.
/// Directory admittances are surfaced by [`exec_dirs_line`], `Deny` literals
/// by [`exec_denies`], so only admitted literals land here.
fn exec_line(caps: &Capabilities) -> String {
    let Some(m) = &caps.exec else {
        return "unrestricted".into();
    };
    let admitted: Vec<String> = m
        .literals
        .iter()
        .filter_map(|(name, pol)| pol.admit_label(name))
        .collect();
    if admitted.is_empty() {
        "(none)".into()
    } else {
        admitted.join(", ")
    }
}

/// Allowed directory prefixes, each with a trailing `/` so it reads as a
/// directory.  Empty when nothing admits by directory.
fn exec_dirs_line(caps: &Capabilities) -> String {
    caps.exec.as_ref().map_or_else(String::new, |m| {
        m.allow_dirs
            .iter()
            .map(|dir| format!("{}/", dir.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// Names and directory prefixes carrying an explicit `Deny` — vetoed even
/// where a covering directory admittance would otherwise admit the path.
fn exec_denies(caps: &Capabilities) -> Vec<String> {
    caps.exec.as_ref().map_or_else(Vec::new, |m| {
        let literals = m
            .literals
            .iter()
            .filter(|&(_, pol)| pol.is_denied())
            .map(|(name, _)| name.clone());
        let dirs = m.deny_dirs.iter().map(|dir| format!("{}/", dir.as_str()));
        literals.chain(dirs).collect()
    })
}

fn or_none<S: AsRef<str>>(v: &[S]) -> String {
    if v.is_empty() {
        "(none)".into()
    } else {
        join_str(v)
    }
}

/// Comma-join any slice whose items borrow as `&str` — the prefix lists hold
/// `NormalizedPrefix`es, not strings.
fn join_str<S: AsRef<str>>(v: &[S]) -> String {
    v.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(", ")
}

/// The Skills section: `name: description` per skill, the same
/// progressive-disclosure shape as [`builtin_index`].
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

    /// The name set `builtin_index` lists, split out of its preamble so a test
    /// asserts membership without depending on order.
    fn names(index: &str) -> HashSet<&str> {
        let list = index.split("\n\n").nth(1).expect("index has a name list");
        list.split(", ").collect()
    }

    #[test]
    fn builtin_index_lists_reply_and_schedule_family_when_both_bits_hold() {
        let shell = crate::bootstrap::boot_shell();
        let index = builtin_index(&shell, true, true);
        let n = names(&index);
        assert!(n.contains("reply"));
        assert!(n.contains("schedule"));
        assert!(n.contains("schedules"));
        assert!(n.contains("unschedule"));
    }

    /// Non-returning: the interactive trunk and every `/branch` child.
    #[test]
    fn builtin_index_omits_reply_for_a_non_returning_agent() {
        let shell = crate::bootstrap::boot_shell();
        let index = builtin_index(&shell, false, true);
        let n = names(&index);
        assert!(
            !n.contains("reply"),
            "must not advertise a verb the desk always refuses"
        );
        assert!(
            n.contains("schedule"),
            "a granted agent still holds the schedule family"
        );
    }

    #[test]
    fn builtin_index_omits_schedule_family_for_an_ungranted_agent() {
        let shell = crate::bootstrap::boot_shell();
        let index = builtin_index(&shell, true, false);
        let n = names(&index);
        assert!(!n.contains("schedule"));
        assert!(!n.contains("schedules"));
        assert!(!n.contains("unschedule"));
        assert!(n.contains("reply"), "a returning agent still holds `reply`");
    }

    /// The default interactive trunk.
    #[test]
    fn builtin_index_omits_both_families_when_neither_bit_holds() {
        let shell = crate::bootstrap::boot_shell();
        let index = builtin_index(&shell, false, false);
        let n = names(&index);
        assert!(!n.contains("reply"));
        assert!(!n.contains("schedule"));
        assert!(!n.contains("schedules"));
        assert!(!n.contains("unschedule"));
    }

    #[test]
    fn builtin_indexes_apply_substitutes_the_placeholder() {
        let shell = crate::bootstrap::boot_shell();
        let indexes = BuiltinIndexes::resolve(&shell);
        let template = format!("before\n\n{BUILTIN_INDEX_PLACEHOLDER}\n\nafter");
        let resolved = indexes.apply(&template, false, true);
        assert_eq!(
            resolved,
            format!("before\n\n{}\n\nafter", builtin_index(&shell, false, true))
        );
        assert!(!resolved.contains(BUILTIN_INDEX_PLACEHOLDER));
    }

    /// The no-op property [`BuiltinIndexes::apply`]'s doc lets callers rely on.
    #[test]
    fn builtin_indexes_apply_is_a_noop_without_the_placeholder() {
        let shell = crate::bootstrap::boot_shell();
        assert_eq!(
            BuiltinIndexes::resolve(&shell).apply("plain text", true, true),
            "plain text"
        );
    }
}
