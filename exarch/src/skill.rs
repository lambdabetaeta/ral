//! Agent Skills: progressive-disclosure skill loading per the
//! [Agent Skills spec](https://agentskills.io/llms.txt).
//!
//! Skills are directories containing a `SKILL.md` file with YAML
//! frontmatter (`name`, `description`) and Markdown body. They are
//! discovered from two roots:
//!
//! - `.exarch/skills/` in the working directory (project skills)
//! - `$XDG_CONFIG_HOME/exarch/skills/` (user skills)
//!
//! ## Progressive disclosure
//!
//! 1. The prompt carries a static Skills section — `name: description`
//!    per skill, discovered and grant-filtered once at startup. The body
//!    is withheld until asked for.
//! 2. `skill-list` re-scans at call time, filtered by the live grant —
//!    picks up skills added mid-session.
//! 3. `skill <name>` loads the full `SKILL.md` body, also at call
//!    time — picks up skills added or edited mid-session.

use gray_matter::Matter;
use gray_matter::engine::YAML;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A discovered skill — enough metadata for the prompt and enough path
/// info for on-demand body loading.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// Discover all skill directories from both roots — directory names only,
/// no file reads.
///
/// The caller parses frontmatter after a grant check.
/// Returns `(name, dir)` pairs where `name` is the directory basename.
/// Duplicate names: local (cwd) overrides global (config).
pub fn discover_all(cwd: &Path, config_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();
    for root in [
        cwd.join(".exarch").join("skills"),
        config_dir.join("skills"),
    ] {
        for (name, dir) in scan_dir(&root) {
            if seen.insert(name.clone()) {
                skills.push((name, dir));
            }
        }
    }
    skills
}

/// Discover skills from both roots, filter by `caps` readability, and
/// parse frontmatter — for the prompt's Skills section.
///
/// Called once at
/// startup (pre-turn); file reads here are `silent`, gated by the
/// static capability set rather than a live shell.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:skill-metadata] reads SKILL.md frontmatter at prompt assembly to build the Skills section; pre-turn, gated by caps"
)]
pub fn discover_metadata(
    cwd: &Path,
    config_dir: &Path,
    caps: &ral_core::types::Capabilities,
) -> Vec<Skill> {
    let mut skills = Vec::new();
    for (name, dir) in discover_all(cwd, config_dir) {
        if !dir_readable(&dir, caps) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(dir.join("SKILL.md")) else {
            continue;
        };
        if let Some(skill) = skill_from_frontmatter(&raw, &name) {
            skills.push(skill);
        }
    }
    skills
}

/// Check whether `dir` is within any read prefix and not within any
/// deny path of `caps`.  `None` fs policy = unrestricted = readable.
fn dir_readable(dir: &Path, caps: &ral_core::types::Capabilities) -> bool {
    let Some(fs) = &caps.fs else {
        return true;
    };
    let in_read = fs
        .read_prefixes
        .iter()
        .any(|p| ral_core::path::path_within(dir, p.as_path()));
    if !in_read {
        return false;
    }
    !fs.deny_paths
        .iter()
        .any(|p| ral_core::path::path_within(dir, p.as_path()))
}

/// Build a [`Skill`] from a `SKILL.md`'s raw text: parse the YAML frontmatter
/// and accept it only when the declared `name` matches the directory it lives in
/// (`dir_name`) and is a valid skill name.  I/O-free — callers do their own
/// `read_to_string` under the appropriate io-door, so the silent (startup) and
/// surface (turn-time) reads stay distinct doors while sharing this parse.
fn skill_from_frontmatter(raw: &str, dir_name: &str) -> Option<Skill> {
    let matter = Matter::<YAML>::new();
    let parsed: gray_matter::ParsedEntity<gray_matter::Pod> = matter.parse(raw).ok()?;
    let data = parsed.data?;
    let name = data["name"].as_string().ok()?;
    let description = data["description"].as_string().ok()?;
    (name == dir_name && valid_skill_name(&name)).then_some(Skill { name, description })
}

/// Read a `SKILL.md` and parse its frontmatter into a [`Skill`].  `dir_name` is
/// the expected skill name (the parent directory).  Called at turn time by
/// `skill-list`, gated by `check_fs_read` at the call site.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:skill-list] reads a SKILL.md's frontmatter for the `skill-list` builtin; the surface card justifies the read"
)]
pub(crate) fn parse_skill(path: &Path, dir_name: &str) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    skill_from_frontmatter(&raw, dir_name)
}

/// Read the Markdown body of `SKILL.md` — everything after the
/// frontmatter.  Called by the `skill` builtin at turn time, gated by
/// `check_fs_read` at the call site.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:skill-body] reads the SKILL.md body for `skill`; the surface card justifies the read"
)]
pub(crate) fn read_skill_body(dir: &Path) -> Result<String, String> {
    let path = dir.join("SKILL.md");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let matter = Matter::<YAML>::new();
    let parsed: gray_matter::ParsedEntity<gray_matter::Pod> = matter
        .parse(&raw)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(parsed.content)
}

// ---------------------------------------------------------------------------
// internal
// ---------------------------------------------------------------------------

/// `std::fs::read_dir` touches the filesystem but reads no file
/// contents — the caller gates any body/frontmatter reads through the
/// grant.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:skill-list-dir] lists skill directory names; no file contents read — frontmatter/body reads are gated by check_fs_read"
)]
fn scan_dir(root: &Path) -> Vec<(String, PathBuf)> {
    let mut skills = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return skills;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name.starts_with('.') || !path.is_dir() {
            continue;
        }
        if path.join("SKILL.md").is_file() {
            skills.push((file_name.to_string(), path));
        }
    }
    skills
}

/// Validate a skill name per the Agent Skills spec:
/// - 1–64 characters
/// - lowercase alphanumeric and hyphens only
/// - must not start or end with a hyphen
/// - no consecutive hyphens
///
/// The character set excludes `/` and `.`, so a validated name joined onto
/// a skills root cannot escape it — `skill <name>` leans on this as a
/// path-traversal guard before resolving the directory.
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
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert!(valid_skill_name("pdf"));
        assert!(valid_skill_name("pdf-processing"));
        assert!(valid_skill_name("code-review-2"));
        assert!(valid_skill_name("a"));
    }

    #[test]
    fn invalid_names() {
        assert!(!valid_skill_name(""));
        assert!(!valid_skill_name("-pdf"));
        assert!(!valid_skill_name("pdf-"));
        assert!(!valid_skill_name("pdf--processing"));
        assert!(!valid_skill_name("PDF"));
        assert!(!valid_skill_name("pdf_processing"));
        assert!(!valid_skill_name(&"a".repeat(65)));
    }

    #[test]
    fn parse_valid_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: does things\n---\n\n# Body\ncontent\n",
        )
        .unwrap();
        let skill = parse_skill(&skill_dir.join("SKILL.md"), "my-skill").unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "does things");
    }

    #[test]
    fn name_must_match_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: wrong-name\ndescription: x\n---\n",
        )
        .unwrap();
        assert!(parse_skill(&skill_dir.join("SKILL.md"), "my-skill").is_none());
    }

    #[test]
    fn read_body_after_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: d\n---\n\n# Hello\nworld\n",
        )
        .unwrap();
        let body = read_skill_body(&skill_dir).unwrap();
        assert_eq!(body, "# Hello\nworld");
    }

    /// On a name collision the project-local `.exarch/skills/` wins over
    /// the user's `$XDG_CONFIG_HOME` copy.
    #[test]
    fn local_overrides_global() {
        let cwd = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let local = cwd.path().join(".exarch").join("skills").join("dup");
        let global = config.path().join("skills").join("dup");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(local.join("SKILL.md"), "---\nname: dup\n---\n").unwrap();
        std::fs::write(global.join("SKILL.md"), "---\nname: dup\n---\n").unwrap();

        let found = discover_all(cwd.path(), config.path());
        let dup: Vec<_> = found.iter().filter(|(n, _)| n == "dup").collect();
        assert_eq!(dup.len(), 1, "duplicate name should collapse to one entry");
        assert_eq!(
            dup[0].1, local,
            "the local skill must shadow the global one"
        );
    }
}
