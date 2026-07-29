//! The one door from a path *string* to the names an OS sandbox rule may
//! mention.
//!
//! A rule denotes a set of VFS objects; a path is one name for such an
//! object, and on macOS one object answers to several.  Splicing a raw
//! string into a rule therefore under-enforces on every other spelling — a
//! deny of `/tmp/evil` never matching the `/private/tmp/evil` `execve`
//! checks.  [`Rendered`] is all the emitters accept and [`render_paths`]
//! all that mints one, so the gap cannot reopen.

/// One spelling of a VFS object: a member of a fully expanded name class.
///
/// Private field, no public constructor — bare strings in, opaque names
/// out, which is the whole guarantee.
///
/// Deliberately neither `Serialize` nor `Deserialize`: these are this
/// host's expansion of this host's filesystem, so the missing impls make
/// serialising one a compile error rather than a leak of one machine's
/// canonical forms into another's rules.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rendered(String);

impl Rendered {
    /// The spelling itself, for the emitter splicing it into an OS rule.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Every name by which a kernel sandbox hook might present the objects that
/// `paths` name, deduped, each blessed as [`Rendered`].
///
/// # Errors
///
/// `realpath(3)` can surface a symlink target or mount whose name is not
/// valid Unicode.  An OS rule is a string literal, so a lossy rendering
/// would name a different inode; a grant that cannot be expressed
/// faithfully is refused, not approximated.
#[allow(clippy::disallowed_methods)]
pub fn render_paths<S: AsRef<str>>(paths: &[S]) -> Result<Vec<Rendered>, String> {
    Ok(
        super::canon::match_variants_paths(paths.iter().map(|p| std::path::Path::new(p.as_ref())))?
            .into_iter()
            .map(Rendered)
            .collect(),
    )
}

/// Proper ancestors of already-rendered names, themselves rendered — sorted,
/// deduped, root excluded, exactly as [`proper_ancestors`](super::proper_ancestors)
/// leaves them.
///
/// No re-expansion is owed.  An ancestor of a rendered name is a name the
/// kernel walks while looking up that rendered name, and the leaf's own
/// expansion already put both firmlink spellings into the input set, whose
/// ancestor chains cover each other's toggles.
///
/// Seatbelt alone gates each lookup in the walk separately from the rule on
/// the leaf; bwrap gets the walk from its mounts, and an ACE hangs on the
/// object rather than a name.
#[cfg(target_os = "macos")]
pub(crate) fn rendered_ancestors<'a>(
    paths: impl IntoIterator<Item = &'a Rendered>,
) -> Vec<Rendered> {
    super::proper_ancestors(paths.into_iter().map(Rendered::as_str))
        .into_iter()
        .map(Rendered)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_always_keeps_the_name_the_author_wrote() {
        let v = render_paths(&["/some/non/existent/path"]).expect("ASCII path must be Ok");
        assert!(v.iter().any(|r| r.as_str() == "/some/non/existent/path"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rendering_a_firmlinked_path_yields_both_spellings() {
        let v = render_paths(&["/tmp/ral-render-probe"]).expect("ASCII path must be Ok");
        assert!(v.iter().any(|r| r.as_str() == "/tmp/ral-render-probe"));
        assert!(
            v.iter()
                .any(|r| r.as_str() == "/private/tmp/ral-render-probe")
        );
    }

    /// A name the renderer cannot express faithfully must be refused, never
    /// lossily rendered into a rule naming the wrong inode.  Reaching the
    /// non-UTF-8 expansion needs a non-UTF-8 *input*, which `S: AsRef<str>`
    /// forbids, so the door is exercised through a real symlink whose target
    /// carries the invalid bytes.
    ///
    /// Linux-only because APFS validates filenames as UTF-8 and returns
    /// `EILSEQ`, so the situation cannot be staged on macOS; there the
    /// engine's own refusal tests in `canon` stand in.
    #[cfg(target_os = "linux")]
    #[test]
    fn rendering_refuses_a_path_whose_expansion_is_not_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join(OsStr::from_bytes(b"\xFFghost"));
        std::fs::create_dir(&target).expect("non-UTF-8 target dir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let err = render_paths(&[link.to_str().expect("temp path is UTF-8")])
            .expect_err("a non-UTF-8 expansion must fail closed");
        assert!(err.contains("not valid UTF-8"), "got {err:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ancestors_of_rendered_names_exclude_the_root_and_are_deduped() {
        let leaves = [
            Rendered("/a/b/c".to_string()),
            Rendered("/a/b/d".to_string()),
        ];
        let got = rendered_ancestors(&leaves);
        assert_eq!(
            got.iter().map(Rendered::as_str).collect::<Vec<_>>(),
            ["/a", "/a/b"]
        );
    }
}
