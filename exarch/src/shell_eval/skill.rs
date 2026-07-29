//! Agent Skills: a directory holding a `SKILL.md`, YAML frontmatter (`name`,
//! `description`) over a Markdown body, discovered from `.exarch/skills/` in the
//! cwd and from `$XDG_CONFIG_HOME/exarch/skills/`.
//!
//! Progressive disclosure: the prompt's Skills section carries only
//! `name: description`, baked once at startup by `prompt::assemble`, while the
//! `skill-list` and `skill` builtins rescan at call time — so a skill added or
//! edited mid-session is still found, and a body reaches the model only when
//! asked for.

use gray_matter::Matter;
use gray_matter::engine::YAML;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A skill's frontmatter — the `name: description` pair the prompt lists.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

/// The two skill roots in precedence order: the cwd's `.exarch/skills/`, then
/// the user's `$XDG_CONFIG_HOME/exarch/skills/`.
pub(crate) fn skill_roots(cwd: &Path, config_dir: &Path) -> [PathBuf; 2] {
    [
        cwd.join(".exarch").join("skills"),
        config_dir.join("skills"),
    ]
}

/// Every skill directory under both roots as `(name, dir)`, the name being
/// the basename; on a collision the cwd root shadows the config one.
///
/// Reads no file contents, so callers gate their own frontmatter and body
/// reads.
pub fn discover_all(cwd: &Path, config_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();
    for root in skill_roots(cwd, config_dir) {
        for (name, dir) in scan_dir(&root) {
            if seen.insert(name.clone()) {
                skills.push((name, dir));
            }
        }
    }
    skills
}

/// Frontmatter for every readable skill, for the prompt's Skills section.
///
/// Runs once at boot, before a `Shell` exists, so it filters against the
/// static `caps` where `skill-list` and `skill` use `Shell::check_fs_read`.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:skill-metadata] reads SKILL.md frontmatter at prompt assembly to build the Skills section; pre-run, gated by caps"
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

/// Whether `caps` admits reading `dir`; no fs policy is the lattice top, so
/// unrestricted and readable.
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

/// Parse a `SKILL.md`'s frontmatter, accepting it only when the declared `name`
/// is valid and matches its directory.  I/O-free by design: callers do their own
/// read, keeping the silent startup door and the surface turn-time door distinct
/// while sharing this parse.
fn skill_from_frontmatter(raw: &str, dir_name: &str) -> Option<Skill> {
    let matter = Matter::<YAML>::new();
    let parsed: gray_matter::ParsedEntity<gray_matter::Pod> = matter.parse(raw).ok()?;
    let data = parsed.data?;
    let name = data["name"].as_string().ok()?;
    let description = data["description"].as_string().ok()?;
    (name == dir_name && valid_skill_name(&name)).then_some(Skill { name, description })
}

/// `dir`'s frontmatter, for the `skill-list` builtin; the call site has already
/// cleared `check_fs_read`.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:skill-list] reads a SKILL.md's frontmatter for the `skill-list` builtin; the surface card justifies the read"
)]
pub(crate) fn parse_skill(dir: &Path, dir_name: &str) -> Option<Skill> {
    let raw = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    skill_from_frontmatter(&raw, dir_name)
}

/// The Markdown after the frontmatter, for the `skill` builtin; the call site
/// has already cleared `check_fs_read`.
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
        let skill = parse_skill(&skill_dir, "my-skill").unwrap();
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
        assert!(parse_skill(&skill_dir, "my-skill").is_none());
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
