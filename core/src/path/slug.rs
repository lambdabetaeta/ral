//! Directory-name slugs of absolute paths, for keying per-project state by the
//! directory a process was launched in.

use super::site::Silent;
use crate::site::site;
use sha2::{Digest, Sha256};
use std::path::{Component, Path};

/// Name an absolute path as one directory-name component: its readable part,
/// a `-`, and its identity.
///
/// The readable part is for a person scanning a listing.  `/Users/x/proj`
/// becomes `-Users-x-proj`, the leading `-` standing in for the root, built on
/// [`std::path::Path::components`] so `\` and drive prefixes come out right
/// too.  It is lossy: names are joined with `-` and read as text where they
/// are not Unicode, so `/a-b/c` and `/a/b-c` share one.
///
/// The identity is 16 lowercase hex digits, the first 8 bytes of the SHA-256
/// of the path's exact OS bytes: its bytes on Unix, and on Windows each UTF-16
/// unit as two little-endian bytes.  Two paths that differ at all therefore
/// get different keys, short of a 64-bit collision.  On Unix, `/Users/x/proj`
/// is `-Users-x-proj-25cf7293a3713b73`.
#[must_use]
pub fn project_slug(cwd: &Path) -> String {
    format!("{}-{}", readable_slug(cwd), identity_hash(cwd))
}

/// The readable part of [`project_slug`]: each name of `cwd` joined with `-`.
fn readable_slug(cwd: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut rooted = false;
    for comp in site!(Silent::SlugKey, cwd => components()) {
        match comp {
            Component::Prefix(p) => {
                let drive: String = site!(Silent::SlugKey, p.as_os_str() => to_string_lossy())
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                if !drive.is_empty() {
                    parts.push(drive);
                }
            }
            Component::RootDir => rooted = true,
            Component::Normal(s) => {
                parts.push(site!(Silent::SlugKey, s => to_string_lossy()).into_owned());
            }
            Component::CurDir | Component::ParentDir => {}
        }
    }
    let body = parts.join("-");
    let slug = if rooted { format!("-{body}") } else { body };
    if slug.is_empty() { "_".into() } else { slug }
}

/// The identity part of [`project_slug`]: the first 8 bytes of the SHA-256 of
/// `cwd`'s exact OS bytes, as 16 lowercase hex digits.
fn identity_hash(cwd: &Path) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    hasher.update(std::os::unix::ffi::OsStrExt::as_bytes(cwd.as_os_str()));
    #[cfg(windows)]
    for unit in std::os::windows::ffi::OsStrExt::encode_wide(cwd.as_os_str()) {
        hasher.update(unit.to_le_bytes());
    }
    let first = hasher
        .finalize()
        .iter()
        .take(8)
        .fold(0u64, |acc, &b| acc << 8 | u64::from(b));
    format!("{first:016x}")
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::project_slug;
    use std::path::Path;

    // The identity is a digest of the OS bytes, which differ by host: the
    // bytes on Unix, UTF-16 little-endian on Windows.
    fn identity(unix: &'static str, windows: &'static str) -> &'static str {
        if cfg!(windows) { windows } else { unix }
    }

    #[test]
    fn slug_is_the_readable_part_then_the_identity() {
        assert_eq!(
            project_slug(Path::new("/Users/x/ral-private")),
            format!(
                "-Users-x-ral-private-{}",
                identity("0a9fb7c47159a96e", "6f638c284f259eff")
            )
        );
        assert_eq!(
            project_slug(Path::new("/")),
            format!("--{}", identity("8a5edab282632443", "ade0274f84b828b0"))
        );
    }

    #[test]
    fn paths_with_one_readable_part_get_distinct_slugs() {
        let (a, b) = (
            project_slug(Path::new("/a-b/c")),
            project_slug(Path::new("/a/b-c")),
        );
        assert!(a.starts_with("-a-b-c-") && b.starts_with("-a-b-c-"));
        assert_ne!(a, b);
    }
}
