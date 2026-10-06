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
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct Rendered(String);

impl Rendered {
    /// The spelling itself, for the emitter splicing it into an OS rule.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `spelled`, taken as written, is this name or lies under it.
    #[cfg(unix)]
    pub(crate) fn holds(&self, spelled: &str) -> bool {
        super::lex::path_within_str(spelled, &self.0, super::lex::Identity::Stored)
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
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn render_paths<S: AsRef<str>>(paths: &[S]) -> Result<Vec<Rendered>, String> {
    Ok(
        super::canon::match_variants_paths(paths.iter().map(|p| std::path::Path::new(p.as_ref())))?
            .into_iter()
            .map(Rendered)
            .collect(),
    )
}

/// `real` and its firmlink twin, never re-resolved: a frozen grant names
/// what it named, not what a symlink since put there reaches.
///
/// # Errors
///
/// As [`render_paths`].
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn render_real(real: &super::RealPath) -> Result<Vec<Rendered>, String> {
    let names = std::iter::once(real.as_path());
    Ok(super::canon::spelled(names, |p| {
        super::canon::with_firmlink_twins(vec![p.to_path_buf()])
    })?
    .into_iter()
    .map(Rendered)
    .collect())
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

/// The directories a deny needs kept traversable to be reachable at all:
/// every proper ancestor of a rendered deny name that lies within some
/// rendered write name — taken *after* expansion, so a deny reached only
/// through a symlinked chain (`W/alias/secret` where `W/alias → W/top/deep`)
/// pins every ancestor of both the surface and the resolved spelling, not
/// just the ones the pre-expansion string happened to share with `write`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn rendered_pins(deny: &[Rendered], write: &[Rendered]) -> Vec<Rendered> {
    super::proper_ancestors(deny.iter().map(Rendered::as_str))
        .into_iter()
        .filter(|dir| {
            write
                .iter()
                .any(|w| super::lex::path_within_str(dir, w.as_str(), super::lex::Identity::Stored))
        })
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

    /// A frozen real path whose name has since become a symlink renders to
    /// that name alone, never to where the link now leads.
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods, reason = "[test] fs scaffolding")]
    #[test]
    fn a_real_path_since_symlinked_never_renders_its_new_target() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let allowed = tmp.path().join("allowed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&allowed).expect("allowed");
        std::fs::create_dir(&outside).expect("outside");
        let real = super::super::RealPath::of(&allowed).expect("allowed is real");
        std::fs::remove_dir(&allowed).expect("remove");
        std::os::unix::fs::symlink(&outside, &allowed).expect("symlink");

        let outside = outside.canonicalize().expect("outside exists");
        for name in render_real(&real).expect("ASCII path renders") {
            assert!(
                !std::path::Path::new(name.as_str()).starts_with(&outside),
                "{name:?} lies under {outside:?}"
            );
        }
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
