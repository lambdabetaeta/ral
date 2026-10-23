//! How ral shows a path: `/`-separated on every host.
//!
//! For diagnostics and display only, never identity or serialisation: lossy on
//! non-Unicode names, not byte-injective (`\` and `/` both render as `/` on
//! Windows), and host-dependent (`dunce` simplifies verbatim prefixes on
//! Windows alone, and `\` is folded only under Windows rules).

use super::PathRules;
use super::lex::parse_bytes;
use super::site::Silent;
use crate::site::site;
use std::borrow::Cow;
use std::path::Path;

/// `p` as ral shows it.
pub fn shown(p: impl AsRef<Path>) -> String {
    fold_for(
        &site!(Silent::Shown, dunce::simplified(p.as_ref()) => to_string_lossy()),
        PathRules::HOST,
    )
    .into_owned()
}

/// The separator fold: a Windows path reads `/`-separated unless it opens
/// with a [namespace head](super::lex::Parsed::has_os_namespace_head) as the OS
/// returns one, past which `/` is not a separator.
pub(crate) fn fold_for(s: &str, rules: PathRules) -> Cow<'_, str> {
    match rules {
        PathRules::Windows
            if !parse_bytes(s.as_bytes(), PathRules::Windows).has_os_namespace_head() =>
        {
            Cow::Owned(s.replace('\\', "/"))
        }
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
