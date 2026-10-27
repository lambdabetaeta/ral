//! The prompt contains only guest truths and uses exarch's shared renderer.

use exarch::app::SYNOD;
use ral_core::own::UserFile;
use ral_core::path::{XdgKind, shown};
use std::path::Path;

/// Trusted config, never the granted folder; instructions cannot widen authority.
const HOUSE_RULES: &str = "house-rules.md";

pub(crate) fn assemble(workspace: &Path, folder_name: &str) -> Result<String, String> {
    let mut sections: Vec<(Option<&str>, String)> = Vec::new();
    sections.push((None, include_str!("../../data/system.md").into()));
    sections.push((
        Some("Ral"),
        format!(
            "{}\n{}",
            exarch::prompt::RAL_REFERENCE,
            include_str!("../../data/office-examples.md")
        ),
    ));
    sections.push((Some("Editing"), exarch::prompt::EDIT_REPLACE.into()));
    sections.push((
        Some("Toolbox"),
        include_str!("../../data/toolbox.md").into(),
    ));
    sections.push((
        Some("The folder"),
        format!(
            "{}\nThe folder granted for this session is mounted at `{}`; outside it there is only the scratch space named under Host. To the user this folder is `{folder_name}` — speak of it by that name, never by its mounted path.\n",
            include_str!("../../data/folder.md"),
            shown(workspace)
        ),
    ));
    sections.push((
        Some("Network"),
        include_str!("../../data/network.md").into(),
    ));
    sections.push((Some("Host"), host_section(workspace)));
    let rules = UserFile::xdg(SYNOD.name(), XdgKind::Config, HOUSE_RULES)?;
    if let Some(text) = rules.read()? {
        sections.push((Some("House rules"), text));
    }
    sections.push((
        Some("Talking to the user"),
        include_str!("../../data/surface.md").into(),
    ));
    Ok(exarch::prompt::render(&sections))
}

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
        exarch::app::SCRATCH_VAR,
        exarch::prompt::SCRATCH_PLACEHOLDER
    )
}
