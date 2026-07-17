//! Assemble the system prompt from its parts.
//!
//! Each entry is a `(heading, body)` pair; one renderer walks them
//! uniformly so the shape of the prompt is the shape of the Vec built
//! in `assemble`.  Headed sections get a `# heading` line; the persona
//! section is unheaded — it sets the tone, not a topic.

pub mod host;

use crate::cli::EditScheme;
use crate::shell_eval::skill;
use ral_core::Shell;
use ral_core::types::{Capabilities, ExecDir};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// The stand-in system prompt for `--chat` mode ([`crate::cli::Cli::chat`]).
///
/// Chat obliterates the assembled prompt entirely; this is not a persona but
/// the minimal non-empty string, present only because some provider backends
/// (the Codex/Responses adapter) reject an empty system prompt, and Anthropic
/// rejects a whitespace-only one ("text content blocks must contain
/// non-whitespace text").  A single period satisfies both, uniform across every
/// provider — chat does not branch on the adapter.
pub const CHAT_SYSTEM: &str = ".";

/// Build the full system prompt as an ordered list of `(heading, body)`
/// sections that [`render`] walks uniformly.  The order, and what each
/// section carries:
///
/// 1. **persona** (unheaded) — baked `data/system.md`, or the `--system
///    FILE...` files when given.  `--system` replaces *only* the persona;
///    every other section stands.
/// 2. **Ral** — the language and tool reference (`data/ral.md`).
/// 3. **Editing** — the file-editing scheme `edit` selects: line-hash
///    (`data/edit-hash.md`) or string-replace (`data/edit-replace.md`).
///    Only the prompt text switches; both builtins stay registered.
/// 4. **Builtins** — every builtin/prelude function's name only, a
///    progressive-disclosure index (see [`builtin_index`]). This function
///    bakes in a *placeholder* for the index, not the index itself: the
///    real, per-agent list — filtered to the verbs that agent actually
///    holds — is resolved by [`resolve_builtin_index`] once
///    [`crate::agent::Agent::assemble`] has the agent's own `returns` and
///    `allow_schedule` bits in reach. Every other section is agent-invariant
///    and stands as rendered here.
/// 5. **Tasks** — the task-management kit API (`data/tasks.md`).
/// 6. **Script style** — the scripting guide (`data/script-style.md`).
/// 7. **Host** — the environment snapshot ([`host::snapshot`]) and the live
///    grant, one under the other.
/// 8. **Workspace** (optional) — the discovered `AGENTS.md` chain (see
///    [`discover_agents`]): operator config first, then repo root down to
///    cwd, deepest last.  Instructions, not authority.
/// 9. **Skills** (optional) — `name: description` per discovered readable
///    skill, with a note to call `skill <name>` to load and `skill-list` to
///    refresh mid-session.
/// 10. **Agent** / **Surfacing** — the closing section, chosen by `headless`:
///     a headless run gets the `reply` return-channel contract
///     (`data/agent.md`); an interactive run gets the surfacing guidance
///     (`data/surface.md`).  Last, where its recency carries.
///
/// # Errors
/// Returns `Err` if reading a `--system` file or a discovered workspace
/// `AGENTS.md` fails.
pub fn assemble(
    files: &[PathBuf],
    caps: &Capabilities,
    scratch: &Path,
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

/// [`assemble`]'s stand-in for the per-agent builtin index — never sent to a
/// model, always resolved by [`resolve_builtin_index`] before an [`Agent`
/// ](crate::agent::Agent) is fully constructed.  A template holds this
/// placeholder rather than the index itself precisely because the index is
/// no longer agent-invariant: `assemble` bakes the rest of the prompt once,
/// at boot, before any agent's own `returns`/`allow_schedule` bits exist to
/// filter by.
pub(crate) const BUILTIN_INDEX_PLACEHOLDER: &str = "@@EXARCH_BUILTIN_INDEX@@";

/// Every command *this* agent can name, as one comma-separated line of
/// **names**: `shell`'s own installed builtins (core's plus exarch's own
/// surface — `view-text`, `grep-files`, `edit-hash`, … — everything
/// [`bootstrap::boot_shell`](crate::bootstrap::boot_shell) dresses the shell
/// with before any agent is assembled), the documented prelude functions,
/// and the agent library (`view-text-around`, which rides in as part of the
/// prelude). Sorted and deduped, with `_`-prefixed internals filtered out —
/// note [`Shell::builtin_names`] does *not* drop the `_` names itself, only
/// its callers do, so the filter lives here and covers all three sources.
///
/// Also filtered here: `reply` when `!returns` (the interactive trunk, every
/// `/branch` child — the desk refuses it unconditionally for them), and the
/// self-wakeup family (`schedule`, `schedules`, `unschedule`) when
/// `!allow_schedule` (no `--allow-schedule` grant). Installation stays
/// unconditional and the desk's refusal is the only real wall — this list
/// is prompt-only, so an agent is never shown a verb it cannot call and
/// never spends a turn finding that out.
///
/// This is a *progressive-disclosure* index, not a reference: the agent reads
/// the whole surface at a glance, then `explain <name>`s any one for its
/// signature and docs on demand — baking every help string into the prompt
/// proved far too long. Reading `shell.builtin_names()` directly, rather
/// than naming
/// [`HOST_BUILTIN_SETS`](crate::shell_eval::builtins::HOST_BUILTIN_SETS)
/// here too, means the index is exactly what that agent's shell can
/// dispatch, true by construction: every resolution site calls this only
/// after its shell has run `install_on`, so there is no ordering to get
/// wrong.
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
        "Every builtin and prelude function, by name — call `explain <name>` for any one'\''s signature and docs:\n\n{}",
        names.join(", ")
    )
}

/// Resolve [`assemble`]'s [`BUILTIN_INDEX_PLACEHOLDER`] into the real,
/// per-agent [`builtin_index`] — the one substitution every constructed
/// [`Agent`](crate::agent::Agent) performs on its own `system` text, reading
/// `shell`'s own installed builtins and keyed on the same construction-fixed
/// bits the desk reads for the refusal itself (`returns` for `reply`,
/// `allow_schedule` for the self-wakeup family), so a fresh model never sees
/// a verb the desk will certainly refuse. A no-op on text that never held
/// the placeholder (a `--system` override, a test fixture), so it is safe
/// to run unconditionally.
pub(crate) fn resolve_builtin_index(
    template: &str,
    shell: &Shell,
    returns: bool,
    allow_schedule: bool,
) -> String {
    template.replace(
        BUILTIN_INDEX_PLACEHOLDER,
        &builtin_index(shell, returns, allow_schedule),
    )
}

/// Discover the `AGENTS.md` instruction files to inject, outermost first so
/// the most specific file's recency dominates.  The operator's own
/// `<config>/AGENTS.md` (the trusted XDG config home — the same root
/// [`crate::config`] loads `config.ral` from, never the working tree) leads;
/// then, when `cwd` sits inside a git repository, every `AGENTS.md` from the
/// repo root down to `cwd`; outside a repo, only `cwd/AGENTS.md` (the bare
/// ancestor chain is not followed up into unrelated parents).
///
/// The walk stops at the first ancestor holding a `.git` entry — file or
/// directory, so worktrees and submodules count — which bounds discovery to
/// the project the agent was launched in.  Existence is the only gate,
/// checked through [`ral_core::path::exists`]; the reads happen in
/// [`read_files`], under its door.
///
/// These files steer behaviour, not authority: a cwd `AGENTS.md` lives in the
/// agent's own writable tree, so unlike `config.ral` it is untrusted — but it
/// only adds prompt text, never capabilities, so it cannot widen the `Grant`.
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

/// Concatenate the given files into one section body, blank-line separated and
/// in order.  Serves both the `--system FILE...` files and the discovered
/// `AGENTS.md` chain; the caller fixes the order.
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

/// Render the section list.  Headed sections get a `# heading` line;
/// unheaded sections are emitted verbatim.  Bodies are trimmed and
/// joined by a blank line.
fn render(sections: &[(Option<&str>, String)]) -> String {
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

/// The `Host` section body: the environment snapshot ([`host::snapshot`]) and
/// the live grant, one under the other.  Where the agent stands and when "now"
/// is, then the authority it holds — the facts of its situation, read together.
fn host_section(caps: &Capabilities, scratch: &Path) -> String {
    let state = crate::bootstrap::xdg_app_dir(ral_core::path::basedir::XdgKind::State);
    format!(
        "{}\n{}",
        host::snapshot(&state),
        grant_summary(caps, scratch)
    )
}
/// Render the live grant: a static legend (`data/grant-legend.md`)
/// followed by the live bullet list.  The legend trains the model to
/// read the notation and to recognise the runtime denial string; the
/// bullets carry the actual capabilities.  `None` fields are
/// "unrestricted" (no attenuation at this layer); empty containers
/// are "(none)".  One effect per line so the agent can scan its
/// authority at a glance and avoid burning turns on denied ops.
fn grant_summary(caps: &Capabilities, scratch: &Path) -> String {
    // Ambient authority (e.g. the `dangerous` profile): nothing is
    // attenuated, so the denial-notation legend describes a runtime event
    // that cannot occur here. Collapse the whole section to one line plus
    // the scratch path the agent still needs.
    let ambient = caps.exec.is_none() && caps.fs.is_none() && caps.net != Some(false);
    if ambient {
        return format!(
            "Ambient authority: every command, path, and network call is permitted; \
the sandbox is the trust boundary.\n\n- scratch: `$EXARCH_SCRATCH` = {}\n",
            scratch.display()
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
    let _ = writeln!(s, "- scratch: `$EXARCH_SCRATCH` = {}", scratch.display());
    s
}

/// Per-command exec policy as `name` or `name[sub1,sub2,...]`,
/// comma-joined.  `None` (no exec map) is "unrestricted"; empty
/// literals (or only `Deny` entries) is "(none)".  Directory
/// admittances are surfaced separately by [`exec_dirs_line`]; `Deny`
/// literals by [`exec_denies`].
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

/// Allowed directory prefixes, comma-joined with a trailing `/` to
/// read as directories.  Denied dirs are surfaced by [`exec_denies`],
/// not here.  Empty when no directory admits.
fn exec_dirs_line(caps: &Capabilities) -> String {
    caps.exec.as_ref().map_or_else(String::new, |m| {
        m.dirs
            .iter()
            .filter(|&(_, v)| matches!(v, ExecDir::Allow))
            .map(|(dir, _)| format!("{dir}/"))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// Names and directory prefixes with an explicit `Deny` — vetoed even
/// when a covering directory admittance would otherwise admit the
/// resolved path.  Dir denies keep a trailing `/` to read as directories.
fn exec_denies(caps: &Capabilities) -> Vec<String> {
    caps.exec.as_ref().map_or_else(Vec::new, |m| {
        let literals = m
            .literals
            .iter()
            .filter(|&(_, pol)| pol.is_denied())
            .map(|(name, _)| name.clone());
        let dirs = m
            .dirs
            .iter()
            .filter(|&(_, v)| matches!(v, ExecDir::Deny))
            .map(|(dir, _)| format!("{dir}/"));
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

/// Comma-join any slice whose items borrow as `&str` — the prefix lists
/// hold [`NormalizedPrefix`](ral_core::path::NormalizedPrefix)es, not
/// `&str`, so this helper borrows each as `&str` before joining.
fn join_str<S: AsRef<str>>(v: &[S]) -> String {
    v.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(", ")
}

/// Render the Skills section: one `name: description` line per skill,
/// with a brief note telling the agent how to load full instructions
/// and discover new skills mid-session.
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

    /// The comma-separated name set `builtin_index` actually lists — split
    /// out of its leading "call `explain <name>`..." preamble so a test can
    /// assert membership without depending on name order.
    fn names(index: &str) -> HashSet<&str> {
        let list = index.split("\n\n").nth(1).expect("index has a name list");
        list.split(", ").collect()
    }

    /// A returning, granted agent's index carries every dead-verb candidate
    /// — the common, most-privileged case advertises the full surface.
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

    /// A non-returning agent (the interactive trunk, every `/branch` child)
    /// never sees `reply` named at all — the desk would refuse it
    /// unconditionally, so the index does not advertise it.
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

    /// An ungranted agent never sees any of the self-wakeup family named —
    /// `reply` stands, since a returning agent holds it regardless of the
    /// grant.
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

    /// A non-returning, ungranted agent — the default interactive trunk —
    /// sees neither family at all.
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

    /// [`resolve_builtin_index`] substitutes the placeholder with exactly
    /// [`builtin_index`]'s own output for the given bits.
    #[test]
    fn resolve_builtin_index_substitutes_the_placeholder() {
        let shell = crate::bootstrap::boot_shell();
        let template = format!("before\n\n{BUILTIN_INDEX_PLACEHOLDER}\n\nafter");
        let resolved = resolve_builtin_index(&template, &shell, false, true);
        assert_eq!(
            resolved,
            format!("before\n\n{}\n\nafter", builtin_index(&shell, false, true))
        );
        assert!(!resolved.contains(BUILTIN_INDEX_PLACEHOLDER));
    }

    /// Text that never held the placeholder resolves as a no-op — the
    /// property [`resolve_builtin_index`]'s doc relies on to run safely on
    /// a `--system` override or a bare test fixture.
    #[test]
    fn resolve_builtin_index_is_a_noop_without_the_placeholder() {
        let shell = crate::bootstrap::boot_shell();
        assert_eq!(
            resolve_builtin_index("plain text", &shell, true, true),
            "plain text"
        );
    }
}
