//! Assemble the system prompt as `(heading, body)` sections; the shape of the
//! prompt is the shape of the Vec built in `assemble`.

pub mod host;

use crate::policy::Invocation;
use crate::skill::{self, Boot};
use ral_core::authority::grant::value::{LayerValue, printed};
use ral_core::authority::policy::{Baseline, LocateError};
use ral_core::data::FOValue;
use ral_core::host::EnvVars;
use ral_core::path::CwdPath;
use ral_core::protocol::probe::Grants;
use std::fmt::Write;
use std::path::{Path, PathBuf};

pub const RAL_REFERENCE: &str = include_str!("../data/ral.md");
pub const EDIT_REPLACE: &str = include_str!("../data/edit-replace.md");
const EDIT_HASH: &str = include_str!("../data/edit-hash.md");
const GRANT_LEGEND: &str = include_str!("../data/grant-legend.md");

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
/// If reading a `--system` file or a discovered `AGENTS.md` fails, or the
/// stack refuses a `--system` file.
#[allow(
    clippy::too_many_arguments,
    reason = "the prompt's inputs are the launch's, each read once; the launch has no struct for them"
)]
pub fn assemble(
    files: &[PathBuf],
    invocation: &Invocation,
    baseline: &Baseline,
    app: crate::app::App,
    cwd: &CwdPath,
    config_dir: Option<&Path>,
    interactive: bool,
    edit: EditScheme,
) -> Result<String, String> {
    let env = EnvVars::new();
    let mut reader = Boot::new(&invocation.stack, baseline, &env, cwd);
    let mut sections: Vec<(Option<&str>, String)> = Vec::new();
    // Persona is the only section `--system` replaces; the rest stand.
    sections.push((
        None,
        if files.is_empty() {
            include_str!("../data/system.md").into()
        } else {
            read_files(files, &reader)?
        },
    ));
    sections.push((Some("Ral"), RAL_REFERENCE.into()));
    sections.push((
        Some("Editing"),
        match edit {
            EditScheme::Hash => EDIT_HASH.into(),
            EditScheme::Replace => EDIT_REPLACE.into(),
        },
    ));
    sections.push((Some("Builtins"), BUILTIN_INDEX_PLACEHOLDER.to_string()));
    sections.push((Some("Tasks"), include_str!("../data/tasks.md").into()));
    sections.push((
        Some("Context management"),
        include_str!("../data/context.md").into(),
    ));
    sections.push((Some("Host"), host_section(app)));
    let agents = read_agents(cwd.as_path(), config_dir, &reader)?;
    if !agents.is_empty() {
        sections.push((Some("Workspace"), agents));
    }
    let home_dir = ral_core::host::home_dot("");
    let skills =
        skill::discover_metadata(cwd.as_path(), config_dir, home_dir.as_deref(), &mut reader);
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

/// Fill [`GRANT_PLACEHOLDER`] with the grant an agent's every eval runs
/// under: its engine session's layers.
pub(crate) fn with_grant(template: &str, grants: &Grants) -> String {
    template.replace(GRANT_PLACEHOLDER, &grant_section(grants))
}

/// The `AGENTS.md` chain's text, outermost first so the most specific file's
/// recency dominates: the operator's `<config>/AGENTS.md` — the trusted XDG
/// root [`crate::config`] loads `config.ral` from — then every `AGENTS.md`
/// along [`work_tree_chain`](ral_core::path::work_tree_chain), the repo root
/// down to `cwd`.  Outside a repo, only `cwd/AGENTS.md`.
///
/// The repo files sit in the agent's own writable tree, so unlike `config.ral`
/// they are untrusted: each is a read the stack judges, and one it refuses is
/// skipped and said so, never an error — prompt text is a convenience, as an
/// ignore file is.  They add prompt text, never capabilities, and cannot widen
/// the grant.
///
/// # Errors
/// A file that exists and cannot be read.
fn read_agents(cwd: &Path, config_dir: Option<&Path>, reader: &Boot<'_>) -> Result<String, String> {
    let chain = config_dir
        .into_iter()
        .chain(ral_core::path::work_tree_chain(cwd));
    let mut texts = Vec::new();
    for file in chain.map(|dir| dir.join("AGENTS.md")) {
        match reader.read(&file) {
            Ok(text) => texts.push(text),
            Err(LocateError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(LocateError::Refused(refused)) => {
                ral_core::errln!(
                    "exarch: skipped {}: {refused}",
                    ral_core::path::shown(&file)
                );
            }
            Err(e) => return Err(format!("{}: {e}", ral_core::path::shown(&file))),
        }
    }
    Ok(texts.join("\n\n"))
}

/// Concatenate the `--system` files into one section body, blank-line
/// separated in the order given.
fn read_files(files: &[PathBuf], reader: &Boot<'_>) -> Result<String, String> {
    let texts: Result<Vec<_>, _> = files
        .iter()
        .map(|path| {
            (reader.read(path)).map_err(|e| format!("{}: {e}", ral_core::path::shown(path)))
        })
        .collect();
    Ok(texts?.join("\n\n"))
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
        "You are an agent named '{NAME_PLACEHOLDER}'.\n\n{}\n{GRANT_PLACEHOLDER}- scratch: `${}` = {SCRATCH_PLACEHOLDER}; temporary files go in `!{{env}}[TMPDIR]` under this session scratch. Hardcoded `/tmp` is discarded after each command on Linux and refused on macOS.\n",
        host::snapshot(state.as_deref()),
        crate::app::SCRATCH_VAR
    )
}

/// The grant a stack denotes, as the model reads it.
///
/// The legend of what grants mean, then each layer in ral's own `grant`
/// syntax, oldest first, and the engine host's word where a withheld GPU is
/// not held.  The caller appends the `- scratch:` bullet, since exarch names a seeded
/// env var and host path where synod names the guest's tmpfs.
pub fn grant_section(grants: &Grants) -> String {
    // The denial legend would describe a runtime event that cannot occur.
    if grants.layers.is_empty() {
        return "Ambient authority: every command, path, network call and GPU access is permitted; \
the sandbox is the trust boundary.\n\n"
            .into();
    }
    let layers: Vec<String> = grants.layers.iter().map(layer).collect();
    let mut s = format!(
        "{}\n\n```ral\n{}\n```\n",
        GRANT_LEGEND.trim_end(),
        layers.join("\n")
    );
    let withheld = grants.layers.iter().any(|l| l.grant.field("gpu").is_some());
    if withheld && !grants.withholds_gpu {
        s.push_str(
            "This platform cannot withhold the GPU: `` gpu: `withheld `` leaves it in reach.\n",
        );
    }
    s.push('\n');
    s
}

/// A layer as `grant` writes it, or, where no grant spells it, its grant
/// extended by its fragment.
fn layer(value: &LayerValue) -> String {
    // `grant`'s bracket is the form's: the empty grant is `[]`.
    let grant = match &value.grant {
        FOValue::Map { entries } if entries.is_empty() => "[]".into(),
        grant => printed(grant),
    };
    match &value.fragment {
        None => format!("grant {grant}"),
        Some(fragment) => format!("grant {grant} extended by {}", printed(fragment)),
    }
}

/// Pin the grant section `grants` renders to the file `golden`; with
/// `RAL_BLESS` set, write it there instead.
///
/// # Panics
/// If the section differs from the file, or the file cannot be read or
/// written.
#[cfg(any(test, feature = "test-util"))]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub fn pin_grant_section(golden: &Path, grants: &Grants) {
    let shown = grant_section(grants);
    if std::env::var_os("RAL_BLESS").is_some() {
        std::fs::write(golden, &shown)
            .unwrap_or_else(|e| panic!("bless {}: {e}", golden.display()));
        return;
    }
    let pinned = std::fs::read_to_string(golden)
        .unwrap_or_else(|e| panic!("{}: {e}; `RAL_BLESS=1` writes it", golden.display()));
    assert_eq!(
        shown,
        pinned,
        "the grant section differs from {}",
        golden.display()
    );
}

/// The Skills section: `name: description` per skill, the same
/// progressive-disclosure shape as
/// [`BuiltinIndex`](crate::agent::index::BuiltinIndex).
fn skills_section(skills: &[skill::Skill]) -> String {
    let mut body =
        String::from("Available skills (call `skill <name>` to load, `skill-list` to refresh):\n");
    for s in skills {
        let _ = writeln!(body, "- {}: {}", s.name, s.description);
    }
    body
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::authority::grant::freeze::Place;
    use ral_core::authority::grant::value::{Axes, ExecTable, FsTable, GrantValue, Target};
    use ral_core::authority::policy::{EditorVerb, Gpu, Net};
    use ral_core::data::datum::Datum as _;

    /// A Linux guest's grant as a macOS or Windows host receives it: a value,
    /// its layers a list on the wire, and the guest's word on the GPU beside
    /// them.
    fn crossed(withholds_gpu: bool) -> Grants {
        let work = |sub: &str| Place::Absolute(format!("/work{sub}"));
        let guest = Axes {
            fs: Some(FsTable {
                read: vec![work("")],
                write: vec![work("")],
                deny: vec![work("/.env")],
            }),
            exec: Some(ExecTable {
                allow: vec![
                    Target::Name("git".into()),
                    Target::Dir(Place::Absolute("/usr/bin".into())),
                ],
                deny: vec![Target::Name("bash".into())],
            }),
            net: Some(Net::Offline),
            gpu: Some(Gpu::Withheld),
            editor: Some([EditorVerb::Read].into()),
            shell: None,
        };
        let layer = LayerValue {
            grant: GrantValue::from(guest).into(),
            fragment: None,
        };
        let grants = Grants {
            layers: vec![layer],
            withholds_gpu,
        };
        Grants::decode(&grants.encode()).expect("the answer decodes where it crossed to")
    }

    /// The guest's spellings, whatever the host's own paths look like.
    #[test]
    fn a_crossed_grant_shows_the_guests_spellings_on_every_host() {
        let shown = with_grant(GRANT_PLACEHOLDER, &crossed(true));
        for spelled in [
            "`absolute '/work/.env'",
            "`dir `absolute '/usr/bin'",
            "`name 'git'",
            "`name 'bash'",
            "net: `offline",
            "gpu: `withheld",
        ] {
            assert!(shown.contains(spelled), "{spelled}:\n{shown}");
        }
        assert!(!shown.contains("in reach"), "{shown}");
    }

    /// A host that cannot withhold the GPU says so rather than claim it.
    #[test]
    fn a_gpu_this_platform_cannot_withhold_is_said_to_be_in_reach() {
        let shown = with_grant(GRANT_PLACEHOLDER, &crossed(false));
        assert!(
            shown.ends_with(
                "This platform cannot withhold the GPU: `` gpu: `withheld `` leaves it in reach.\n\n"
            ),
            "{shown}"
        );
    }

    #[test]
    fn a_repeated_layer_is_shown() {
        let mut grants = crossed(true);
        grants.layers.push(grants.layers[0].clone());
        let shown = with_grant(GRANT_PLACEHOLDER, &grants);
        let layers = shown.lines().filter(|line| line.starts_with("grant "));
        assert_eq!(layers.count(), 2, "{shown}");
        assert_eq!(
            shown.matches("`absolute '/work/.env'").count(),
            2,
            "{shown}"
        );
    }

    /// The `AGENTS.md` chain is read through the stack: a link to a file the
    /// stack refuses is skipped, the same link under an allowance is read.
    #[cfg(unix)]
    #[test]
    fn an_agents_link_to_a_denied_file_is_skipped_and_one_under_an_allowance_is_read() {
        use ral_core::authority::policy::{Capabilities, FsPolicy, GrantStack};
        use ral_core::path::{FrozenPath, utf8};
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(root.join("notes.md"), "linked text").unwrap();
        std::os::unix::fs::symlink(root.join("notes.md"), work.join("AGENTS.md")).unwrap();
        let (baseline, env) = (Baseline::default(), EnvVars::new());
        let cwd = CwdPath::new(work.clone()).unwrap();
        let frozen = |path: &Path| FrozenPath::from_surface(utf8(path).unwrap());
        let reading = |deny: Vec<FrozenPath>| {
            ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
                fs: Some(FsPolicy {
                    read_prefixes: vec![frozen(&root)],
                    deny_paths: deny,
                    ..FsPolicy::default()
                }),
                ..Capabilities::default()
            })])
        };
        let agents = |stack: &GrantStack| {
            let reader = Boot::new(stack, &baseline, &env, &cwd);
            read_agents(&work, Some(&root.join("config")), &reader)
        };
        assert_eq!(agents(&reading(Vec::new())), Ok("linked text".to_owned()));
        assert_eq!(
            agents(&reading(vec![frozen(&root.join("notes.md"))])),
            Ok(String::new()),
            "a refused file is skipped, not an error"
        );
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod assembly;
