//! Directory-name slugs of absolute paths, for keying per-project state by the
//! directory a process was launched in.

use std::path::{Component, Path};

/// Slugify an absolute path into one directory-name component.
///
/// `/Users/x/proj` becomes `-Users-x-proj`, the leading `-` standing in for the
/// root.  Built on [`std::path::Path::components`], so `\` and drive prefixes
/// come out right too.
#[must_use]
pub fn project_slug(cwd: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut rooted = false;
    #[allow(
        clippy::disallowed_methods,
        reason = "native: each component of the cwd becomes a slug key, lossy where it is not UTF-8"
    )]
    for comp in cwd.components() {
        match comp {
            Component::Prefix(p) => {
                let drive: String = p
                    .as_os_str()
                    .to_string_lossy()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                if !drive.is_empty() {
                    parts.push(drive);
                }
            }
            Component::RootDir => rooted = true,
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir | Component::ParentDir => {}
        }
    }
    let body = parts.join("-");
    let slug = if rooted { format!("-{body}") } else { body };
    if slug.is_empty() { "_".into() } else { slug }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] fixed fixture paths")]
mod tests {
    use super::project_slug;
    use std::path::Path;

    #[test]
    fn slug_joins_path_components_with_dashes() {
        assert_eq!(
            project_slug(Path::new("/Users/x/ral-private")),
            "-Users-x-ral-private"
        );
        assert_eq!(project_slug(Path::new("/")), "-");
    }
}
