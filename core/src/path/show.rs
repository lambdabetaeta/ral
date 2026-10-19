//! How ral shows a path: `/`-separated on every host.

use super::PathRules;
use std::borrow::Cow;
use std::path::Path;

/// `p` as ral shows it.
pub fn shown(p: impl AsRef<Path>) -> String {
    fold_for(
        &dunce::simplified(p.as_ref()).to_string_lossy(),
        PathRules::HOST,
    )
    .into_owned()
}

/// The separator fold: a Windows path reads `/`-separated unless its head
/// is verbatim or a device, where `/` is not a separator.
pub(crate) fn fold_for(s: &str, rules: PathRules) -> Cow<'_, str> {
    let literal = [r"\\?\", r"\??\", r"\\.\"].iter().any(|h| s.starts_with(h));
    match rules {
        PathRules::Windows if !literal => Cow::Owned(s.replace('\\', "/")),
        _ => Cow::Borrowed(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(s: &str) -> Cow<'_, str> {
        fold_for(s, PathRules::Windows)
    }

    #[test]
    fn windows_paths_read_slash_separated() {
        assert_eq!(win(r"C:\a\b"), "C:/a/b");
        assert_eq!(win(r"\\srv\sh\x"), "//srv/sh/x");
        assert_eq!(win(r"C:a\b"), "C:a/b");
        assert_eq!(win(r"src\a.txt"), "src/a.txt");
    }

    #[test]
    fn verbatim_and_device_heads_stay_as_given() {
        for s in [
            r"\\?\UNC\srv\sh\x",
            r"\\?\C:\COM",
            r"\??\C:\x",
            r"\\.\pipe\x",
        ] {
            assert_eq!(win(s), s);
        }
    }

    #[test]
    fn posix_backslash_is_an_ordinary_byte() {
        assert_eq!(fold_for(r"a\b", PathRules::Posix), r"a\b");
    }
}
