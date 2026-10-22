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
    #[cfg(target_os = "linux")]
    pub(crate) fn holds(&self, spelled: &str) -> bool {
        super::identity::path_within_str(spelled, &self.0)
    }
}

impl AsRef<str> for Rendered {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Every name by which a kernel sandbox hook might present the objects at
/// `paths`, resolved ones, deduped, each blessed as [`Rendered`].
///
/// # Errors
///
/// An OS rule is a string literal, so a path that is not valid Unicode
/// would be rendered lossily as a different inode; a grant that cannot be
/// expressed faithfully is refused, not approximated.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: lifts resolved strings for `match_variants_paths` to name"
)]
#[cfg(test)]
pub(crate) fn render_paths<S: AsRef<str>>(paths: &[S]) -> Result<Vec<Rendered>, String> {
    Ok(
        super::canon::match_variants_paths(paths.iter().map(|p| std::path::Path::new(p.as_ref())))?
            .into_iter()
            .map(Rendered)
            .collect(),
    )
}

/// One object a frozen allowance names: its real path, and the other names
/// the kernel presents it by or it is shown at: its firmlink twin on macOS,
/// the surface spelling it was granted by on Linux, a destination there.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Object {
    pub(crate) real: Rendered,
    pub(crate) spellings: Vec<Rendered>,
    /// What a lookup of each surface it was granted by stats on the way, on
    /// macOS, where Seatbelt gates the `lstat` of every link passed.  No name
    /// of the object: nothing beneath one is reached through it.
    pub(crate) walk: Vec<Rendered>,
}

#[cfg(unix)]
impl Object {
    /// Every name of the object, the real path first.
    pub(crate) fn names(&self) -> impl Iterator<Item = &Rendered> {
        std::iter::once(&self.real).chain(&self.spellings)
    }
}

/// Each frozen allowance as its object, one entry per object.  The object is
/// the frozen real path through [`render_real`], never re-resolved; the
/// surface a prefix was granted by is only where Linux shows it, and the walk
/// macOS lets a lookup take.
///
/// # Errors
///
/// As [`render_paths`].
#[cfg(unix)]
pub(crate) fn render_objects<'a>(
    grants: impl IntoIterator<Item = (&'a super::RealPath, Option<&'a super::FrozenPath>)>,
) -> Result<Vec<Object>, String> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut objects = BTreeMap::<Rendered, [BTreeSet<Rendered>; 2]>::new();
    for (position, surface) in grants {
        let mut names = render_real(position)?.into_iter();
        let Some(real) = names.next() else { continue };
        let shown = surface
            .filter(|_| cfg!(target_os = "linux"))
            .map(|surface| Rendered(surface.as_str().to_owned()))
            .filter(|surface| *surface != real);
        let walk = surface
            .filter(|_| cfg!(target_os = "macos"))
            .map(|surface| render_walk(surface, position));
        let [spellings, walked] = objects.entry(real).or_default();
        spellings.extend(names.chain(shown));
        walked.extend(walk.transpose()?.into_iter().flatten());
    }
    Ok(objects
        .into_iter()
        .map(|(real, [spellings, walk])| Object {
            real,
            spellings: spellings.into_iter().collect(),
            walk: walk.into_iter().collect(),
        })
        .collect())
}

/// Every name a lookup of `real`, granted by `surface`, stats
/// ([`lookup_walk`](super::canon::lookup_walk)), with firmlink twins.
#[cfg(unix)]
fn render_walk(
    surface: &super::FrozenPath,
    real: &super::RealPath,
) -> Result<Vec<Rendered>, String> {
    let names = super::canon::lookup_walk(surface.surface_path(), real.as_path());
    Ok(
        super::canon::spelled(names.iter().map(std::path::PathBuf::as_path), |p| {
            super::canon::with_firmlink_twins(vec![p.to_path_buf()])
        })?
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

/// The positions an exclusion holds: its name's and, where that is a link,
/// the link's target's.
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
    /// lossily rendered into a rule naming the wrong inode.
    ///
    /// Linux-only because APFS validates filenames as UTF-8 and returns
    /// `EILSEQ`, so the situation cannot be staged on macOS; there the
    /// engine's own refusal tests in `canon` stand in.
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] a non-UTF-8 fixture path handed to the renderer"
    )]
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
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods, reason = "[test] fs scaffolding")]
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
    #[allow(clippy::disallowed_methods, reason = "[test] fs scaffolding")]
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
        let objects = render_objects([(&real, Some(&link)), (&real, None), (&real, Some(&link))])
            .expect("ASCII paths render");
        assert_eq!(
            objects,
            [Object {
                real: shown(real.as_path()),
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
