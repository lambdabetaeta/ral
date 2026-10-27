//! Assemble the system prompt as `(heading, body)` sections; the shape of the
//! prompt is the shape of the Vec built in `assemble`.

pub mod host;

use crate::policy::Invocation;
use crate::skill::{self, Boot};
use ral_core::authority::policy::{Baseline, ExecSummary, GrantSummary, LayerSummary, LocateError};
use ral_core::host::EnvVars;
use ral_core::path::CwdPath;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};

pub const RAL_REFERENCE: &str = include_str!("../data/ral.md");
pub const EDIT_REPLACE: &str = include_str!("../data/edit-replace.md");
const EDIT_HASH: &str = include_str!("../data/edit-hash.md");

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
pub(crate) fn with_grant(template: &str, grants: &GrantSummary) -> String {
    template.replace(GRANT_PLACEHOLDER, &grant_summary(grants))
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
/// A static legend teaching the notation and the runtime denial string, then
/// each layer a [`GrantSummary`] shows, oldest first, then the net and gpu
/// lines once. The summary carries the engine host's word on whether a
/// withheld GPU is held there.
///
/// `None` is "unrestricted" (no attenuation at this layer) and an empty
/// container "(none)".  The caller appends the `- scratch:` bullet, since
/// exarch names a seeded env var and host path where synod names the guest's
/// tmpfs.
fn grant_summary(grants: &GrantSummary) -> String {
    let layers = grants.layers();
    // The denial legend would describe a runtime event that cannot occur.
    if layers.is_empty() {
        return "Ambient authority: every command, path, network call and GPU access is permitted; \
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
    for (i, layer) in layers.iter().enumerate() {
        if layers.len() > 1 {
            let _ = writeln!(s, "\nLayer {}:", i + 1);
        }
        s.push_str(&layer_lines(layer));
    }
    let denied = layers.iter().any(|l| l.net.is_some());
    let _ = writeln!(s, "\n- net: {}", if denied { "deny" } else { "inherit" });
    let gpu = match (
        layers.iter().any(|l| l.gpu.is_some()),
        grants.withholds_gpu(),
    ) {
        (false, _) => "inherit",
        (true, true) => "withheld",
        (true, false) => "in reach: this platform cannot withhold the GPU",
    };
    let _ = writeln!(s, "- gpu: {gpu}");
    s
}

fn legend() -> String {
    format!("{}\n\n", include_str!("../data/grant-legend.md").trim_end())
}

/// One layer's bullets bar net, gpu and scratch.
fn layer_lines(layer: &LayerSummary) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "- exec: {}", exec_line(layer.exec.as_ref()));
    let dirs = layer.exec.as_ref().map_or(vec![], allowed_dirs);
    if !dirs.is_empty() {
        let _ = writeln!(s, "- exec dirs: {}", dirs.join(", "));
    }
    let denies = layer.exec.as_ref().map_or(&[][..], |exec| &exec.denies);
    if !denies.is_empty() {
        let _ = writeln!(s, "- exec deny: {}", denies.join(", "));
    }
    if let Some(fs) = &layer.fs {
        let _ = writeln!(s, "- fs read: {}", or_none(&fs.read));
        let _ = writeln!(s, "- fs write: {}", or_none(&fs.write));
        if !fs.deny.is_empty() {
            let _ = writeln!(s, "- fs deny: {}", join_str(&fs.deny));
        }
    }
    if let Some(verbs) = &layer.editor {
        let _ = writeln!(s, "- editor: {}", verbs_line(verbs));
    }
    if let Some(verbs) = &layer.shell {
        let _ = writeln!(s, "- shell: {}", verbs_line(verbs));
    }
    s
}

/// A directory allowance as a grant spells it.
const DIR_KEY: &str = "`dir ";

/// Allowed directories, each rendered as its `dir` target.  Empty when
/// nothing allows by directory.
fn allowed_dirs(exec: &ExecSummary) -> Vec<&str> {
    (exec.allows.iter())
        .map(String::as_str)
        .filter(|k| k.starts_with(DIR_KEY))
        .collect()
}

/// Admitted commands, comma-joined: bare names, then path keys as their
/// author spelled them.  Directory allowances are surfaced by
/// [`allowed_dirs`], exclusions by the layer's `exec deny`, so only the other
/// allowed keys land here.  Beside `every` they add no running, and are
/// listed as authored.
fn exec_line(exec: Option<&ExecSummary>) -> String {
    let Some(exec) = exec else {
        return "unrestricted".into();
    };
    let admitted: Vec<&str> = (exec.allows.iter())
        .map(String::as_str)
        .filter(|k| !k.starts_with(DIR_KEY))
        .collect();
    match (exec.every, admitted.is_empty()) {
        (true, true) => "every program, less the exclusions below".into(),
        (true, false) => format!(
            "every program, less the exclusions below; also named: {}",
            admitted.join(", ")
        ),
        (false, true) => "(none)".into(),
        (false, false) => admitted.join(", "),
    }
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
    use ral_core::authority::policy::{EditorVerb, FsSummary, Gpu, Net};

    /// A Linux guest's summary as a macOS or Windows host receives it: built
    /// of spellings and axes, its layers a list on the wire, and the guest's
    /// word on the GPU beside them.
    fn crossed(withholds_gpu: bool) -> GrantSummary {
        let layers = vec![LayerSummary {
            fs: Some(FsSummary {
                read: vec!["/work".into()],
                write: vec!["/work".into()],
                deny: vec!["/work/.env".into()],
            }),
            exec: Some(ExecSummary {
                every: false,
                allows: vec!["`name git".into(), "`dir `absolute /usr/bin".into()],
                denies: vec!["`name bash".into()],
            }),
            net: Some(Net::Offline),
            gpu: Some(Gpu::Withheld),
            editor: Some(BTreeSet::from([EditorVerb::Read])),
            shell: None,
        }];
        let wire = serde_json::to_string(&layers).expect("a summary is plain data");
        let layers = serde_json::from_str(&wire).expect("a summary decodes where it crossed to");
        GrantSummary::of(layers, withholds_gpu)
    }

    /// The same lines on every host, whatever the host's own paths look like.
    #[test]
    fn a_crossed_summary_renders_the_same_lines_on_every_host() {
        let shown = with_grant(GRANT_PLACEHOLDER, &crossed(true));
        let lines = "\
- exec: `name git
- exec dirs: `dir `absolute /usr/bin
- exec deny: `name bash
- fs read: /work
- fs write: /work
- fs deny: /work/.env
- editor: read

- net: deny
- gpu: withheld
";
        assert!(shown.ends_with(lines), "{shown}");
    }

    /// A host that cannot withhold the GPU says so rather than claim it.
    #[test]
    fn a_gpu_this_platform_cannot_withhold_is_said_to_be_in_reach() {
        let shown = with_grant(GRANT_PLACEHOLDER, &crossed(false));
        assert!(
            shown.ends_with("- gpu: in reach: this platform cannot withhold the GPU\n"),
            "{shown}"
        );
    }

    #[test]
    fn a_repeated_layer_is_shown() {
        let one = crossed(true);
        let layer = one.layers()[0].clone();
        let grants = GrantSummary::of(vec![layer.clone(), layer], true);
        let shown = with_grant(GRANT_PLACEHOLDER, &grants);
        assert!(shown.contains("the meet of 2 layers"), "{shown}");
        assert!(
            shown.contains("Layer 1:") && shown.contains("Layer 2:"),
            "{shown}"
        );
        assert_eq!(shown.matches("- fs deny: /work/.env").count(), 2, "{shown}");
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
