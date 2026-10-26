//! The names an OS sandbox rule may mention, rendered from the positions a
//! frozen grant holds.
//!
//! A rule denotes a set of VFS objects; a path is one name for such an
//! object, and on macOS one object answers to several.  Splicing a raw
//! string into a rule therefore under-enforces on every other spelling — a
//! deny of `/tmp/evil` never matching the `/private/tmp/evil` `execve`
//! checks.  [`Rendered`] is all the emitters accept, and [`render_real`],
//! [`render_objects`] and [`render_excluded`] all that mint one, each from a
//! position, never re-resolved, so the gap cannot reopen.

#[cfg(target_os = "linux")]
use super::site::Silent;
#[cfg(target_os = "linux")]
use crate::site::site;

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
pub(crate) struct Rendered(String);

impl Rendered {
    /// The spelling itself, for the emitter splicing it into an OS rule.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `spelled`, taken as written, is this name or lies under it.
    #[cfg(target_os = "linux")]
    pub(crate) fn holds(&self, spelled: &str) -> bool {
        super::identity::path_within_str(spelled, &self.0)
    }

    /// Whether `position` is this name or lies under it.
    #[cfg(target_os = "linux")]
    pub(crate) fn holds_position(&self, position: &super::RealPath) -> bool {
        super::identity::path_within(
            position.as_path(),
            self.0.as_ref(),
            super::identity::Identity::Stored,
        )
    }

    /// Whether this name is `position` or lies under it.
    #[cfg(target_os = "linux")]
    pub(crate) fn under_position(&self, position: &super::RealPath) -> bool {
        super::identity::path_within(
            self.0.as_ref(),
            position.as_path(),
            super::identity::Identity::Stored,
        )
    }

    /// This name as a mount of `from` shows it at `onto`; `None` where `from`
    /// does not hold it.  Lexical: the kernel's names, not a judgment.
    #[cfg(target_os = "linux")]
    pub(crate) fn rebased(&self, from: &Self, onto: &Self) -> Option<Self> {
        let name = site!(Silent::RenderedRebase, std::path::Path::new(&self.0));
        let beneath = site!(Silent::RenderedRebase, name => strip_prefix(&from.0)).ok()?;
        if beneath.as_os_str().is_empty() {
            return Some(onto.clone());
        }
        let shown = site!(Silent::RenderedRebase, std::path::Path::new(&onto.0)).join(beneath);
        Some(Self(
            shown
                .into_os_string()
                .into_string()
                .expect("two rendered names join to one"),
        ))
    }
}

impl AsRef<str> for Rendered {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// One object a frozen allowance names: its real path, and the other names
/// the kernel presents it by or it is shown at: its firmlink twin on macOS,
/// the surface spelling it was granted by on Linux, a destination there.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Object {
    /// The position the allowance holds, for the renderer that opens it.
    pub(crate) position: super::RealPath,
    pub(crate) real: Rendered,
    pub(crate) spellings: Vec<Rendered>,
    /// What the walk of each surface it was granted by passed
    /// ([`Trace::passed`](super::Trace::passed)), on macOS: Seatbelt lets a
    /// lookup stat directories and read links, and reach nothing beneath one.
    pub(crate) walk: Vec<Rendered>,
}

impl Object {
    /// Every name of the object, the real path first.
    pub(crate) fn names(&self) -> impl Iterator<Item = &Rendered> {
        std::iter::once(&self.real).chain(&self.spellings)
    }
}

/// Each frozen allowance as its object, one entry per object.  The object is
/// the frozen real path through [`render_real`], never re-resolved; the
/// surface a prefix was granted by is only where Linux shows it, and what its
/// walk passed is only what macOS lets a lookup traverse.
///
/// # Errors
///
/// As [`render_real`].
pub(crate) fn render_objects<'a>(
    grants: impl IntoIterator<
        Item = (
            &'a super::RealPath,
            Option<&'a super::FrozenPath>,
            &'a [super::RealPath],
        ),
    >,
) -> Result<Vec<Object>, String> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut objects = BTreeMap::<Rendered, (super::RealPath, [BTreeSet<Rendered>; 2])>::new();
    for (position, surface, passed) in grants {
        let mut names = render_real(position)?.into_iter();
        let Some(real) = names.next() else { continue };
        let shown = surface
            .filter(|_| cfg!(target_os = "linux"))
            .map(|surface| Rendered(surface.as_str().to_owned()))
            .filter(|surface| *surface != real);
        let walk = passed
            .iter()
            .filter(|_| cfg!(target_os = "macos"))
            .map(render_real)
            .collect::<Result<Vec<_>, _>>()?;
        let (_, [spellings, walked]) = objects
            .entry(real)
            .or_insert_with(|| (position.clone(), Default::default()));
        spellings.extend(names.chain(shown));
        walked.extend(walk.into_iter().flatten());
    }
    Ok(objects
        .into_iter()
        .map(|(real, (position, [spellings, walk]))| Object {
            position,
            real,
            spellings: spellings.into_iter().collect(),
            walk: walk.into_iter().collect(),
        })
        .collect())
}

/// `real` and its firmlink twin, never re-resolved: a frozen grant names
/// what it named, not what a symlink since put there reaches.
///
/// # Errors
///
/// An OS rule is a string literal, so a position that is not valid Unicode
/// would be rendered lossily as a different inode; a grant that cannot be
/// expressed faithfully is refused, not approximated.
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

/// The positions an exclusion holds: its name's and, where that is a link,
/// the link's target's.
fn held(excluded: &super::Excluded) -> impl Iterator<Item = &super::RealPath> {
    std::iter::once(excluded.named().real()).chain(excluded.target().map(super::Trace::real))
}

/// Every name by which a kernel sandbox hook might present what `denies`
/// exclude: each held position and its firmlink twin, deduped, never
/// re-resolved.
///
/// # Errors
///
/// As [`render_real`].
pub(crate) fn render_excluded<'a>(
    denies: impl IntoIterator<Item = &'a super::Excluded>,
) -> Result<Vec<Rendered>, String> {
    let mut names = std::collections::BTreeSet::new();
    for real in denies.into_iter().flat_map(held) {
        names.extend(render_real(real)?);
    }
    Ok(names.into_iter().collect())
}

/// The directories a deny needs kept traversable to be reachable at all:
/// every proper ancestor, root excepted, of a position `denies` hold that
/// lies within some `writes` prefix, rendered with its firmlink twin.  A deny
/// reached through a symlinked chain pins the chain it resolves to, the one
/// the kernel walks.
///
/// # Errors
///
/// As [`render_real`].
pub(crate) fn pinned_dirs<'a>(
    denies: impl IntoIterator<Item = &'a super::Excluded>,
    writes: &[super::RealPath],
) -> Result<Vec<Rendered>, String> {
    let pins: std::collections::BTreeSet<_> = denies
        .into_iter()
        .flat_map(held)
        .flat_map(|real| {
            writes
                .iter()
                .flat_map(move |write| real.ancestors_within(write))
        })
        .filter(|dir| !dir.is_root())
        .collect();
    let mut names = std::collections::BTreeSet::new();
    for dir in &pins {
        names.extend(render_real(dir)?);
    }
    Ok(names.into_iter().collect())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn rendered(position: &str) -> Vec<String> {
        render_real(&super::super::RealPath::unwalked_for_test(position))
            .expect("an ASCII position renders")
            .into_iter()
            .map(|r| r.as_str().to_owned())
            .collect()
    }

    #[test]
    fn rendering_keeps_the_position() {
        assert!(
            rendered("/some/non/existent/path").contains(&"/some/non/existent/path".to_owned())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rendering_a_firmlinked_position_yields_both_spellings() {
        let v = rendered("/tmp/ral-render-probe");
        assert!(v.contains(&"/tmp/ral-render-probe".to_owned()));
        assert!(v.contains(&"/private/tmp/ral-render-probe".to_owned()));
    }

    /// A name the renderer cannot express faithfully must be refused, never
    /// lossily rendered into a rule naming the wrong inode.
    ///
    /// Linux-only because APFS validates filenames as UTF-8 and returns
    /// `EILSEQ`, so the situation cannot be staged on macOS; there the
    /// engine's own refusal tests in `canon` stand in.
    #[cfg(target_os = "linux")]
    #[test]
    fn rendering_refuses_a_position_that_is_not_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join(OsStr::from_bytes(b"\xFFghost"));
        std::fs::create_dir(&target).expect("non-UTF-8 target dir");

        let err = render_real(&crate::test_helper::real(&target))
            .expect_err("a non-UTF-8 position must fail closed");
        assert!(err.contains("not valid UTF-8"), "got {err:?}");
    }

    /// A frozen real path whose name has since become a symlink renders to
    /// that name alone, never to where the link now leads.
    #[test]
    fn a_real_path_since_symlinked_never_renders_its_new_target() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let allowed = tmp.path().join("allowed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&allowed).expect("allowed");
        std::fs::create_dir(&outside).expect("outside");
        let real = crate::test_helper::real(&allowed);
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

    /// A prefix granted through a symlink is the object its frozen real path
    /// names, shown at the spelling it was granted by, however many grants
    /// name the object.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_frozen_prefix_is_its_real_object_shown_at_its_surface() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("usr-bin");
        std::fs::create_dir(&dir).expect("dir");
        let link = tmp.path().join("bin");
        std::os::unix::fs::symlink(&dir, &link).expect("symlink");
        let link = super::super::FrozenPath::from_surface(link.to_str().expect("UTF-8"));
        let real = crate::test_helper::real(&dir);
        let shown = |name: &std::path::Path| Rendered(name.to_str().expect("UTF-8").to_owned());
        let none: &[super::super::RealPath] = &[];
        let objects = render_objects([
            (&real, Some(&link), none),
            (&real, None, none),
            (&real, Some(&link), none),
        ])
        .expect("ASCII paths render");
        assert_eq!(
            objects,
            [Object {
                real: shown(real.as_path()),
                position: real,
                spellings: vec![Rendered(link.as_str().to_owned())],
                walk: Vec::new(),
            }]
        );
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
