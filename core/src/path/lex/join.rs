//! A subpath joined onto a base name by name, and a suffix appended to a
//! name.

use std::path::{Path, PathBuf};

use super::head::{absolute_under, has_drive, is_sep_byte};
use crate::path::PathRules;

/// A subpath that would not join *onto* its base but replace it, as
/// `Path::join` lets it: one naming a volume of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rerooting;

/// `sub`, split on the separators of `rules`, joined onto `base` under
/// `rules`, a string read alike from any host.
///
/// The names join with the separator of `rules`, where `base.join(sub)` on
/// Windows would leave `C:\x\ral/rc`, and where a host `Path::join` would
/// read them under the host's rules, not the given ones.  An empty
/// component, from a leading, doubled or trailing separator or an empty
/// `sub`, adds nothing, so a root of the current volume, `/x`, or `\x` under
/// Windows, joins under `base`.  `.` and `..` join as written, for the
/// resolver to fold.  A separator goes between `base` and the first name
/// unless `base` is empty, ends with one, or, under Windows rules, is a bare
/// drive: `C:` is that drive's current directory, and a name in it is
/// spelled `C:x`, as Windows spells one there.
///
/// # Errors
/// [`Rerooting`] when `sub` names a volume of its own, which only Windows
/// rules have: `sub` is [absolute](absolute_under) there, opening with two
/// separators as a share or a namespace head does (`\\srv\sh`, `//srv/sh`,
/// `\\?\UNC\srv\sh`, `\\.\pipe\x`), or a drive root; or a component at any
/// position carries a drive (`C:`, `C:x`), which re-roots a join even once
/// separators are split out.  Posix rules refuse nothing: every root there
/// is the one root, `//x` included, and `C:` a name.
pub(crate) fn join_under(base: &str, sub: &str, rules: PathRules) -> Result<String, Rerooting> {
    let mut joined = base.to_owned();
    joined.push_str(&appended(base.as_bytes(), sub, rules)?);
    Ok(joined)
}

/// [`join_under`] under the host's rules, onto a path the host holds, whose
/// name need not be UTF-8.
///
/// # Errors
/// [`join_under`]'s.
pub(crate) fn join_relative(base: impl AsRef<Path>, sub: &str) -> Result<PathBuf, Rerooting> {
    let base = base.as_ref().as_os_str();
    let mut joined = base.to_os_string();
    joined.push(appended(base.as_encoded_bytes(), sub, PathRules::HOST)?);
    Ok(PathBuf::from(joined))
}

/// What [`join_under`] appends to `base`: each name of `sub`, a separator
/// of `rules` before it where one is due.
fn appended(base: &[u8], sub: &str, rules: PathRules) -> Result<String, Rerooting> {
    let names = || {
        sub.split(|c| rules.is_separator(c))
            .filter(|c| !c.is_empty())
    };
    if rules == PathRules::Windows
        && (absolute_under(sub, PathRules::Windows) || names().any(has_drive))
    {
        return Err(Rerooting);
    }
    let mut open = match base {
        [] => true,
        [d, b':'] if rules == PathRules::Windows => d.is_ascii_alphabetic(),
        [.., last] => is_sep_byte(rules, *last),
    };
    let mut tail = String::new();
    for name in names() {
        if !open {
            tail.push(char::from(rules.separator()));
        }
        tail.push_str(name);
        open = false;
    }
    Ok(tail)
}

/// `sub` spelled relative, as [`join_under`] joins a subpath it accepts
/// onto a base: its leading separators under `rules` come off, so `/etc`
/// names `etc` under the base, never `/etc`.  Under Posix a leading
/// backslash is a filename byte, and stays.
pub(crate) fn relative_suffix(sub: &str, rules: PathRules) -> &str {
    sub.trim_start_matches(|c| rules.is_separator(c))
}

/// [`join_relative`] for a subpath the program spells, `/`-separated
/// whatever the host: being constant, it is relative or a bug.
///
/// # Panics
/// If `sub` re-roots, which a relative constant cannot.
pub fn join_slash(base: impl AsRef<Path>, sub: &'static str) -> PathBuf {
    join_relative(base, sub)
        .unwrap_or_else(|Rerooting| panic!("the constant subpath {sub:?} re-roots its join"))
}

/// `base` with `.ext` appended: `build.ps1` + `EXE` is `build.ps1.EXE`,
/// `ledger.json` + `tmp` is `ledger.json.tmp`.
///
/// Concatenation on the `OsStr`, never `Path::with_extension`, which replaces
/// an extension the name already has rather than adding one.
pub fn with_appended_suffix(base: &Path, ext: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// The counterpart of [`with_appended_suffix`]: `path` less a trailing
/// `.ext`, `ledger.json.tmp` less `tmp` being `ledger.json`; `None` when the
/// name does not end in `.ext` after a non-empty stem.
///
/// One extension of `ext` at a time, by `Path::extension`, so the stem is cut
/// on the `OsStr` and a name that is not UTF-8 strips like any other.
pub fn strip_appended_suffix(path: &Path, ext: &str) -> Option<PathBuf> {
    ext.rsplit('.').try_fold(path.to_path_buf(), |stem, piece| {
        (stem.extension()? == piece).then(|| stem.with_extension(""))
    })
}
