//! Assemble the system prompt from its parts.
//!
//! Each entry is a `(heading, body)` pair; one renderer walks them
//! uniformly so the shape of the prompt is the shape of the Vec built
//! in `assemble`.  Headed sections get a `# heading` line; the persona
//! section is unheaded — it sets the tone, not a topic.

use crate::host;
use ral_core::types::{Capabilities, ExecDir};
use std::path::{Path, PathBuf};

/// Build the full system prompt.  Order: persona, Grant, Host, Ral,
/// Script style, [Workspace], [Headless].  Grant sits directly under the persona so
/// its constraints are encountered before the tool reference (`Ral`)
/// tempts the model to use capabilities it does not hold.  The set of
/// builtins is not listed here; the agent discovers it at runtime with
/// `help`, which reads the live resolver and so cannot drift out of
/// date.  With no `--system` files the bake-in persona
/// (`data/system.md`), ral guide (`data/ral.md`), and scripting guide
/// (`data/script-style.md`) appear as distinct sections; `--system
/// FILE...` collapses them into one user-supplied section and the
/// `Ral` / `Script style` slots are omitted (the user takes
/// responsibility for the tool reference).  A `Workspace` section follows,
/// present whenever any `AGENTS.md` is discovered (see [`discover_agents`]):
/// the operator's own `<config>/AGENTS.md` then every repo `AGENTS.md` from
/// the git root down to the cwd, the deepest last so its recency wins.  It
/// carries project instructions, not authority — it cannot widen the `Grant`
/// — and appears regardless of `--system`, the same orthogonality the
/// headless note relies on.  When `headless`, a closing
/// section (`data/headless.md`) tells the root it returns its result by
/// calling `reply` exactly once (a finish without `reply` returns nothing
/// and fails) — appended last, where its recency carries, and regardless of
/// `--system` since it governs the return channel, not the persona.
pub fn assemble(
    files: &[PathBuf],
    caps: &Capabilities,
    scratch: &Path,
    cwd: &Path,
    config_dir: &Path,
    headless: bool,
) -> Result<String, String> {
    let mut sections: Vec<(Option<&str>, String)> = Vec::new();
    sections.push((
        None,
        if files.is_empty() {
            include_str!("../data/system.md").into()
        } else {
            read_files(files)?
        },
    ));
    sections.push((Some("Grant"), grant_summary(caps, scratch)));
    sections.push((Some("Host"), host::snapshot()));
    if files.is_empty() {
        sections.push((Some("Ral"), include_str!("../data/ral.md").into()));
        sections.push((
            Some("Script style"),
            include_str!("../data/script-style.md").into(),
        ));
    }
    let agents = discover_agents(cwd, config_dir);
    if !agents.is_empty() {
        sections.push((Some("Workspace"), read_files(&agents)?));
    }
    if headless {
        sections.push((Some("Headless"), include_str!("../data/headless.md").into()));
    }
    Ok(render(&sections))
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
    let repo_root = cwd
        .ancestors()
        .find(|dir| ral_core::path::exists(&dir.join(".git").to_string_lossy()));
    let mut scan: Vec<&Path> = match repo_root {
        Some(root) => {
            let mut dirs: Vec<&Path> = cwd.ancestors().take_while(|dir| *dir != root).collect();
            dirs.push(root);
            dirs
        }
        None => vec![cwd],
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
    s.push_str(&format!("- exec: {}\n", exec_line(caps)));
    let dirs = exec_dirs_line(caps);
    if !dirs.is_empty() {
        s.push_str(&format!("- exec dirs: {dirs}\n"));
    }
    let denies = exec_denies(caps);
    if !denies.is_empty() {
        s.push_str(&format!("- exec deny: {}\n", denies.join(", ")));
    }
    if let Some(fs) = &caps.fs {
        s.push_str(&format!("- fs read: {}\n", or_none(&fs.read_prefixes)));
        s.push_str(&format!("- fs write: {}\n", or_none(&fs.write_prefixes)));
        if !fs.deny_paths.is_empty() {
            s.push_str(&format!("- fs deny: {}\n", join_str(&fs.deny_paths)));
        }
    }
    s.push_str(&format!(
        "- net: {}\n",
        match caps.net {
            None => "inherit",
            Some(true) => "allow",
            Some(false) => "deny",
        }
    ));
    s.push_str(&format!(
        "- scratch: `$EXARCH_SCRATCH` = {}\n",
        scratch.display()
    ));
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
