//! Assemble synod's system prompt from its parts.
//!
//! The shape is exarch's: an ordered list of `(heading, body)` pairs walked
//! by one renderer, so the Vec built in [`assemble`] *is* the prompt.  Synod
//! borrows the renderer itself ([`exarch::prompt::render`]) and the grant
//! placeholder ([`exarch::prompt::GRANT_PLACEHOLDER`]) rather than growing a
//! second copy of either; what it does not borrow is the *voice*, which is
//! the whole difference between a coding agent and an office one — nor the
//! situation report, whose every line is a host truth this agent, alone in
//! its own machine, must never be told.

use ral_core::path::shown;
use std::path::Path;

/// The office house rules, if the operator deployed any: a plain-language
/// file the office's own staff can write — letter templates, naming
/// conventions, who signs what — read from the trusted config directory,
/// never from the granted folder.  Instructions, not authority: the text
/// steers behaviour and cannot widen the grant.
const HOUSE_RULES: &str = "house-rules.md";

/// Build synod's system prompt as an ordered list of `(heading, body)`
/// sections, rendered by [`exarch::prompt::render`].  The order, and what
/// each section carries:
///
/// 1. **persona** (unheaded, `data/system.md`) — an office assistant who
///    works by scripting but never speaks of it.
/// 2. **Ral** — the language reference, `include_str!`'d straight out of
///    exarch's `data/ral.md`, plus synod's own `data/office-examples.md`
///    demonstrating table inspection and verified document conversion.
/// 3. **Editing** — exarch's `data/edit-replace.md`, again by
///    `include_str!`.  Synod has no `--edit` flag and so must *choose*: the
///    line-hash scheme exists to keep a coding agent's many small line edits
///    honest against a drifting source file, which is not synod's work.
///    Synod's documents are `.xlsx` and `.docx` — opaque to a line editor
///    and manipulated wholesale through Python and `LibreOffice` — and its
///    text edits are few, targeted, and to files it usually just wrote.
///    String-replace needs no prior `view-hash` to obtain a witness, which
///    is one fewer step on every edit, and half the prompt weight for a
///    peripheral capability.
/// 4. **Toolbox** (`data/toolbox.md`) — how office work is actually done
///    with the image's userland.  This section is what makes synod capable.
/// 5. **The folder** (`data/folder.md`) — the workspace contract, plus
///    where the folder stands: mounted at `workspace` (the guest path,
///    the only one that names anything in the machine the agent runs in)
///    and called `folder_name` by the user, which is how the agent must
///    speak of it.  The host path appears nowhere: a model handed one
///    will use it in programs, and inside the guest it names nothing.
///    Edits land in the user's real documents at
///    once; there is no accept gate by design, and nothing is ever put
///    back.  All the host keeps is an account of what changed — the
///    folder's shape before and after — which is its business, never the
///    model's.  So this section must never suggest a change is pending
///    review or reversible: a model that believes in a gate, or in an
///    undo, is a model that overwrites originals cheerfully.
/// 6. **Network** (`data/network.md`) — a fixed package-site allowlist, not
///    the web.
/// 7. **Host** — [`host_section`], synod's own: guest truths only.
///    Exarch's section reports this process's cwd, user, `HOME` and git
///    state — host facts that are all false inside the machine the agent
///    actually works in — so synod composes when now is, where the agent
///    stands, and the scratch space around the shared grant placeholder
///    ([`exarch::prompt::GRANT_PLACEHOLDER`]), which the trunk fills from
///    its own engine.
/// 8. **House rules** (optional) — `<config_dir>/house-rules.md`.
/// 9. **Talking to the user** (`data/surface.md`) — the card grammar and
///    the register to write in.  Last, where its recency carries.
///
/// # Errors
/// Returns `Err` if `<config_dir>/house-rules.md` exists but cannot be read.
pub fn assemble(workspace: &Path, folder_name: &str, config_dir: &Path) -> Result<String, String> {
    let mut sections: Vec<(Option<&str>, String)> = Vec::new();
    sections.push((None, include_str!("../data/system.md").into()));
    sections.push((
        Some("Ral"),
        format!(
            "{}\n{}",
            include_str!("../../exarch/data/ral.md"),
            include_str!("../data/office-examples.md")
        ),
    ));
    sections.push((
        Some("Editing"),
        include_str!("../../exarch/data/edit-replace.md").into(),
    ));
    sections.push((Some("Toolbox"), include_str!("../data/toolbox.md").into()));
    sections.push((
        Some("The folder"),
        format!(
            "{}\nThe folder granted for this session is mounted at `{}`; outside it there is only the scratch space named under Host. To the user this folder is `{folder_name}` — speak of it by that name, never by its mounted path.\n",
            include_str!("../data/folder.md"),
            shown(workspace)
        ),
    ));
    sections.push((Some("Network"), include_str!("../data/network.md").into()));
    sections.push((Some("Host"), host_section(workspace)));
    let rules = config_dir.join(HOUSE_RULES);
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:house-rules] loading synod's own optional house-rules config file \
                  while building the prompt; not the model's turn-time I/O"
    )]
    match std::fs::read_to_string(&rules) {
        Ok(text) => sections.push((Some("House rules"), text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", ral_core::path::shown(rules))),
    }
    sections.push((
        Some("Talking to the user"),
        include_str!("../data/surface.md").into(),
    ));
    Ok(exarch::prompt::render(&sections))
}

/// Synod's `Host` section: guest truths only, around the shared grant
/// summary.
///
/// The agent runs inside the machine, so the facts of its situation are
/// the machine's — the image's Linux userland, the mount point it starts
/// in, and the scratch its engine names, filled in once the engine is up.
/// Only `now` is taken from the host, because the machine's clock is set
/// from it and a letter must carry today's date.
fn host_section(workspace: &Path) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "- os: Linux (Ubuntu userland)");
    if let Some(now) = exarch::prompt::host::now() {
        let _ = writeln!(s, "- now: {now}");
    }
    let _ = writeln!(s, "- cwd: {}", shown(workspace));
    format!(
        "{s}\n{}- scratch: `${}` = {} — this machine's own temporary space, gone when the conversation ends\n",
        exarch::prompt::GRANT_PLACEHOLDER,
        exarch::app::EXARCH.scratch_var(),
        exarch::prompt::SCRATCH_PLACEHOLDER
    )
}
