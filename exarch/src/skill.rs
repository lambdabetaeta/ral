//! Agent Skills from shared and exarch-specific directories, and those
//! compiled into exarch.
//!
//! Progressive disclosure: the prompt's Skills section carries only
//! `name: description`, baked once at startup by `prompt::assemble`, while the
//! `skill-list` and `skill` builtins rescan at call time — so a skill added or
//! edited mid-session is still found, and a body reaches the model only when
//! asked for.

use gray_matter::Matter;
use gray_matter::engine::YAML;
use ral_core::Shell;
use ral_core::authority::policy::LocateError;
use ral_core::authority::policy::{Baseline, GrantStack};
use ral_core::host::EnvVars;
use ral_core::path::{
    CwdPath, Dir, Judged, Kind, LeafMode, LexicalPath, Located, Reading, Resolver,
};
use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// A skill's frontmatter — the `name: description` pair the prompt lists.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

/// Project roots before personal roots; exarch's own wins within each scope.
/// Shared project roots run from cwd up to the working tree's root. Every root
/// comes before [`BUILTIN`].
pub(crate) fn skill_roots(
    cwd: &Path,
    config_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> Vec<PathBuf> {
    std::iter::once(cwd.join(".exarch").join("skills"))
        .chain(
            ral_core::path::work_tree_chain(cwd)
                .into_iter()
                .rev()
                .map(|dir| dir.join(".agents").join("skills")),
        )
        .chain(config_dir.map(|dir| dir.join("skills")))
        .chain(home_dir.map(|dir| dir.join(".agents").join("skills")))
        .collect()
}

/// Who judges a skill read: a shell at call time, where a refusal is the
/// trail's, and at boot a stack and the baseline of the runtime the boot built.
pub(crate) trait Judge {
    /// `path` as the spelling the judge walks.
    fn anchor(&self, path: &Path) -> LexicalPath;
    /// The regular file `rp` names, held for reading; `None` where it is
    /// absent, not a file, or refused.
    fn file(&mut self, rp: &LexicalPath) -> Option<Located<Judged<Reading>>>;
    /// The directory `rp` names, held for reading; `None` where it is absent
    /// or refused.
    fn dir(&mut self, rp: &LexicalPath) -> Option<Dir<Judged<Reading>>>;
}

fn is_file(held: &Located<Judged<Reading>>) -> bool {
    held.stat().is_some_and(|stat| stat.kind == Kind::File)
}

impl Judge for Shell {
    fn anchor(&self, path: &Path) -> LexicalPath {
        Self::anchor(self, path)
    }

    fn file(&mut self, rp: &LexicalPath) -> Option<Located<Judged<Reading>>> {
        // A refusal is on the trail already; an absent file is no skill.
        let held = self.locate_existing::<Reading>(rp, LeafMode::Follow);
        held.ok().flatten().filter(is_file)
    }

    fn dir(&mut self, rp: &LexicalPath) -> Option<Dir<Judged<Reading>>> {
        self.locate_dir::<Reading>(rp).ok()
    }
}

/// The boot's reader: no shell exists yet, so the stack this agent will run
/// under judges every read of the boot, with the baseline of the runtime the
/// boot built, and the two judges agree by construction.
pub(crate) struct Boot<'a> {
    grants: &'a GrantStack,
    baseline: &'a Baseline,
    resolver: Resolver<'a>,
}

impl<'a> Boot<'a> {
    /// `env` is empty at boot: discovery roots are already anchored, so the
    /// resolver's home is never consulted.
    pub(crate) fn new(
        grants: &'a GrantStack,
        baseline: &'a Baseline,
        env: &'a EnvVars,
        cwd: &'a CwdPath,
    ) -> Self {
        Self {
            grants,
            baseline,
            resolver: Resolver::at(env, cwd),
        }
    }

    /// The whole of the file `path` names.
    ///
    /// # Errors
    /// The walk's, the read's, or the stack's refusal.
    pub(crate) fn read(&self, path: &Path) -> Result<String, LocateError> {
        let held = self
            .grants
            .locate::<Reading>(self.baseline, &self.resolver.anchor(path))?;
        let mut text = String::new();
        (held.read().map_err(LocateError::Io)?)
            .read_to_string(&mut text)
            .map_err(LocateError::Io)?;
        Ok(text)
    }
}

impl Judge for Boot<'_> {
    fn anchor(&self, path: &Path) -> LexicalPath {
        self.resolver.anchor(path)
    }

    fn file(&mut self, rp: &LexicalPath) -> Option<Located<Judged<Reading>>> {
        let held = self.grants.locate::<Reading>(self.baseline, rp);
        held.ok().filter(is_file)
    }

    fn dir(&mut self, rp: &LexicalPath) -> Option<Dir<Judged<Reading>>> {
        self.grants.locate_dir::<Reading>(self.baseline, rp).ok()
    }
}

/// The skills compiled into exarch, as `(name, SKILL.md)`. They stand behind
/// every root, so a project's or user's skill of the same name shadows one.
pub(crate) const BUILTIN: [(&str, &str); 1] = [(
    "exarch-sandbox",
    include_str!("../data/skills/exarch-sandbox/SKILL.md"),
)];

/// A skill found: its name, and where its `SKILL.md` comes from.
pub(crate) struct Found {
    pub name: String,
    pub source: Source,
}

pub(crate) enum Source {
    /// A directory under a root, with its `SKILL.md` held for reading.
    Disk {
        dir: PathBuf,
        file: Box<Located<Judged<Reading>>>,
    },
    /// A `SKILL.md` compiled into exarch.
    Builtin(&'static str),
}

impl Found {
    /// Where the skill lives, as a reader is shown it.
    pub(crate) fn place(&self) -> String {
        match &self.source {
            Source::Disk { dir, .. } => ral_core::path::shown(dir),
            Source::Builtin(_) => "built into exarch".into(),
        }
    }
}

fn builtins() -> impl Iterator<Item = Found> {
    BUILTIN.iter().map(|&(name, text)| Found {
        name: name.to_owned(),
        source: Source::Builtin(text),
    })
}

/// The built-in skill named `name`, if exarch ships one.
pub(crate) fn builtin(name: &str) -> Option<Found> {
    builtins().find(|found| found.name == name)
}

/// Every readable skill directory under the roots, then the built-ins; the
/// first wins a name collision. Reads no file contents.
pub(crate) fn discover_all(
    cwd: &Path,
    config_dir: Option<&Path>,
    home_dir: Option<&Path>,
    judge: &mut impl Judge,
) -> Vec<Found> {
    let mut seen = HashSet::new();
    skill_roots(cwd, config_dir, home_dir)
        .iter()
        .flat_map(|root| scan_dir(root, judge))
        .chain(builtins())
        .filter(|found| seen.insert(found.name.clone()))
        .collect()
}

/// Frontmatter for every readable skill, for the prompt's Skills section.
///
/// Runs once at boot, before a `Shell` exists, so it judges by the session's
/// whole grant stack ([`Boot`]), the one this agent will run under, as
/// `skill-list` and `skill` judge by the shell.
pub(crate) fn discover_metadata(
    cwd: &Path,
    config_dir: Option<&Path>,
    home_dir: Option<&Path>,
    boot: &mut Boot<'_>,
) -> Vec<Skill> {
    discover_all(cwd, config_dir, home_dir, boot)
        .iter()
        .filter_map(|found| parse_skill(found).ok())
        .collect()
}

/// Parse a `SKILL.md`'s frontmatter, accepting it only when the declared `name`
/// is valid and matches its directory.  I/O-free by design: callers do their own
/// read, keeping the startup judge and the surface turn-time one distinct
/// while sharing this parse.
fn skill_from_frontmatter(raw: &str, dir_name: &str) -> Result<Skill, String> {
    let matter = Matter::<YAML>::new();
    let parsed: gray_matter::ParsedEntity<gray_matter::Pod> = matter
        .parse(raw)
        .map_err(|e| format!("invalid YAML frontmatter: {e}"))?;
    let Some(gray_matter::Pod::Hash(data)) = parsed.data else {
        return Err("missing YAML frontmatter".into());
    };
    // `Pod`'s `Index` panics on a missing key.
    let field = |key| {
        data.get(key)
            .and_then(|v| v.as_string().ok())
            .ok_or_else(|| format!("frontmatter must contain a string `{key}`"))
    };
    let (name, description) = (field("name")?, field("description")?);
    if !valid_skill_name(&name) {
        return Err(format!(
            "invalid skill name {name:?}; use lowercase letters, digits, and hyphens"
        ));
    }
    if name != dir_name {
        return Err(format!(
            "frontmatter name {name:?} must match directory name {dir_name:?}"
        ));
    }
    Ok(Skill { name, description })
}

/// The whole of a skill's `SKILL.md`, read through the handle that was judged.
fn read_skill_file(found: &Found) -> Result<String, String> {
    let held = match &found.source {
        Source::Disk { file, .. } => file,
        Source::Builtin(text) => return Ok((*text).to_owned()),
    };
    let mut raw = String::new();
    let mut file = held.read().map_err(|e| e.to_string())?;
    file.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    Ok(raw)
}

/// A skill's frontmatter, for the `skill-list` builtin and the boot.
pub(crate) fn parse_skill(found: &Found) -> Result<Skill, String> {
    skill_from_frontmatter(&read_skill_file(found)?, &found.name)
}

/// The Markdown after the frontmatter, for the `skill` builtin.
pub(crate) fn read_skill_body(found: &Found) -> Result<String, String> {
    let raw = read_skill_file(found)?;
    let matter = Matter::<YAML>::new();
    let parsed: gray_matter::ParsedEntity<gray_matter::Pod> = matter
        .parse(&raw)
        .map_err(|e| format!("invalid YAML frontmatter: {e}"))?;
    Ok(parsed.content)
}

/// The skills of `root`: each entry that is a directory holding a `SKILL.md`,
/// found by walking it as the entry's name says, so a link to one is one.
fn scan_dir(root: &Path, judge: &mut impl Judge) -> Vec<Found> {
    let Some(dir) = judge.dir(&judge.anchor(root)) else {
        return Vec::new();
    };
    let Ok(entries) = dir.read_dir() else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.name().as_os_str().to_owned().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .filter_map(|name| {
            let dir = root.join(&name);
            let file = judge.file(&judge.anchor(&dir.join("SKILL.md")))?;
            Some(Found {
                name,
                source: Source::Disk {
                    dir,
                    file: Box::new(file),
                },
            })
        })
        .collect()
}

/// An Agent Skills name: 1–64 characters, lowercase alphanumerics and lone
/// interior hyphens.  The charset excludes `/` and `.`, so `skill <name>` joins
/// a validated name onto a root and calls that its path-traversal guard.
pub(crate) fn valid_skill_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    let mut prev_hyphen = false;
    for &b in bytes {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => prev_hyphen = false,
            b'-' => {
                if prev_hyphen {
                    return false;
                }
                prev_hyphen = true;
            }
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
