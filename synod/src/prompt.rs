//! Assemble synod's system prompt from its parts.
//!
//! The shape is exarch's: an ordered list of `(heading, body)` pairs walked
//! by one renderer, so the Vec built in [`assemble`] *is* the prompt.  Synod
//! borrows the renderer itself ([`exarch::prompt::render`]) and the live
//! situation report ([`exarch::prompt::host_section`]) rather than growing a
//! second copy of either; what it does not borrow is the *voice*, which is
//! the whole difference between a coding agent and an office one.

use ral_core::types::Capabilities;
use std::path::Path;

/// The office house rules, if the operator deployed any: a plain-language
/// file the university's own staff can write — letter templates, naming
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
///    exarch's `data/ral.md`, plus synod's own short companion note
///    (`data/ral-note.md`) reading its programming examples across to
///    office material.  The reference is forked nowhere: one language, one
///    document, and a note is cheaper to keep true than a second copy.
/// 3. **Editing** — exarch's `data/edit-replace.md`, again by
///    `include_str!`.  Synod has no `--edit` flag and so must *choose*: the
///    line-hash scheme exists to keep a coding agent's many small line edits
///    honest against a drifting source file, which is not synod's work.
///    Synod's documents are `.xlsx` and `.docx` — opaque to a line editor
///    and manipulated wholesale through Python and `LibreOffice` — and its
///    text edits are few, targeted, and to files it usually just wrote.
///    String-replace needs no prior `view-text` to obtain a witness, which
///    is one fewer turn on every edit, and half the prompt weight for a
///    peripheral capability.
/// 4. **Toolbox** (`data/toolbox.md`) — how office work is actually done
///    with the image's userland.  This section is what makes synod capable.
/// 5. **The folder** (`data/folder.md`) — the workspace contract, plus the
///    granted `root` itself.  Edits land in the user's real documents at
///    once; `SYNOD.md` §4 has no accept gate by design, and its safety net
///    — checkpoint before, undo after — is the host's business, never the
///    model's.  So this section must never suggest a change is pending
///    review or reversible: a model that believes in a gate is a model
///    that overwrites originals cheerfully.
/// 6. **No network** (`data/no-network.md`) — absent, not denied.
/// 7. **Host** — [`exarch::prompt::host_section`], unchanged: where the
///    agent stands, when now is, and the authority it holds.
/// 8. **House rules** (optional) — `<config_dir>/house-rules.md`.
/// 9. **Talking to the user** (`data/surface.md`) — the card grammar and
///    the register to write in.  Last, where its recency carries.
///
/// # Errors
/// Returns `Err` if `<config_dir>/house-rules.md` exists but cannot be read.
pub fn assemble(
    caps: &Capabilities,
    scratch: &exarch::bootstrap::Scratch,
    root: &Path,
    config_dir: &Path,
) -> Result<String, String> {
    let mut sections: Vec<(Option<&str>, String)> = Vec::new();
    sections.push((None, include_str!("../data/system.md").into()));
    sections.push((
        Some("Ral"),
        format!(
            "{}\n{}",
            include_str!("../../exarch/data/ral.md"),
            include_str!("../data/ral-note.md")
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
            "{}\nThe folder granted for this session is `{}`. Every path you read or write is under it; nothing above it exists.\n",
            include_str!("../data/folder.md"),
            root.display()
        ),
    ));
    sections.push((
        Some("No network"),
        include_str!("../data/no-network.md").into(),
    ));
    sections.push((Some("Host"), exarch::prompt::host_section(caps, scratch)));
    let rules = config_dir.join(HOUSE_RULES);
    if ral_core::path::exists(&rules.to_string_lossy()) {
        sections.push((
            Some("House rules"),
            std::fs::read_to_string(&rules).map_err(|e| format!("{}: {e}", rules.display()))?,
        ));
    }
    sections.push((
        Some("Talking to the user"),
        include_str!("../data/surface.md").into(),
    ));
    Ok(exarch::prompt::render(&sections))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every heading [`assemble`] promises, in the order it promises them.
    const HEADINGS: [&str; 7] = [
        "# Ral",
        "# Editing",
        "# Toolbox",
        "# The folder",
        "# No network",
        "# Host",
        "# Talking to the user",
    ];

    /// Wording that would tell the model its work is staged behind a review
    /// step.  `SYNOD.md` §4 deliberately has no such gate — the safety net
    /// is the host's, and a model that believes its work is staged or
    /// reversible will overwrite an original expecting someone to catch the
    /// mistake.  This list is the guard: the prompt is written so that none
    /// of these phrases can appear in it.
    const GATE_CLAIMS: [&str; 6] = [
        "tracked change",
        "accept the change",
        "before it becomes real",
        "awaiting approval",
        "you can undo",
        "private copy",
    ];

    fn prompt() -> String {
        let scratch = exarch::bootstrap::Scratch::for_test(crate::session::SYNOD, "prompt")
            .expect("scratch dir");
        assemble(
            &Capabilities::root(),
            &scratch,
            Path::new("/work"),
            Path::new("/nonexistent-config"),
        )
        .expect("no house rules to read")
    }

    /// The section list is the promised one, in the promised order.
    #[test]
    fn assemble_lays_out_every_promised_section_in_order() {
        let p = prompt();
        let mut at = 0;
        for h in HEADINGS {
            let found = p[at..]
                .find(h)
                .unwrap_or_else(|| panic!("{h} missing, or out of order"));
            at += found + h.len();
        }
        assert!(
            p.starts_with("You are the assistant in synod"),
            "the persona leads, unheaded"
        );
    }

    /// The borrowed sections really carry exarch's own text — a fork would
    /// pass every other test in here silently.
    #[test]
    fn assemble_reuses_exarchs_language_and_editing_documents() {
        let p = prompt();
        assert!(p.contains("`ral` is call-by-push-value"));
        assert!(p.contains("edit-replace PATH FROM TO"));
        assert!(
            !p.contains("edit-hash PATH EDITS"),
            "synod picked one editing scheme; both is worse than either"
        );
    }

    /// The folder section states the v1 contract: a mounted folder, written
    /// for real, with no gate behind it.
    #[test]
    fn assemble_states_the_mounted_folder_and_claims_no_accept_gate() {
        let p = prompt().to_lowercase();
        assert!(p.contains("it is mounted"));
        assert!(p.contains("nothing is pending"));
        assert!(p.contains("`/work`"), "the granted root is named");
        for claim in GATE_CLAIMS {
            assert!(!p.contains(claim), "prompt claims a review gate: {claim:?}");
        }
    }

    /// The office sections are present in substance, not just by heading.
    #[test]
    fn assemble_carries_the_toolbox_and_the_absent_network() {
        let p = prompt();
        assert!(p.contains("openpyxl"));
        assert!(p.contains("ocrmypdf"));
        assert!(p.contains("soffice --headless"));
        assert!(p.contains("no network connection"));
    }

    /// House rules are picked up from the config directory when present, and
    /// their absence is not an error.
    #[test]
    fn assemble_admits_house_rules_from_the_config_directory() {
        let dir = std::env::temp_dir().join(format!("synod-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp config dir");
        std::fs::write(
            dir.join(HOUSE_RULES),
            "Letters are signed by the Registrar.",
        )
        .expect("write house rules");
        let scratch = exarch::bootstrap::Scratch::for_test(crate::session::SYNOD, "house-rules")
            .expect("scratch dir");
        let p = assemble(&Capabilities::root(), &scratch, Path::new("/work"), &dir)
            .expect("house rules read");
        std::fs::remove_dir_all(&dir).expect("clean up");
        assert!(p.contains("# House rules"));
        assert!(p.contains("signed by the Registrar"));
        assert!(!prompt().contains("# House rules"));
    }
}
