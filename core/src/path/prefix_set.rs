//! A set of path prefixes that carries each path in both the *surface*
//! form the author wrote (sigil- and `~`-expanded, `.`/`..` normalised,
//! symlinks intact) and the *resolved* form (symlinks followed).
//!
//! Containment and intersection are judged on the resolved
//! form, so two spellings of one directory (`/tmp` and `/private/tmp`)
//! unify; [`surface`](PrefixSet::surface) returns the surface form, so a
//! consumer that must reproduce the author's path — an OS sandbox
//! profile whose matcher works lexically — emits the spelling the
//! sandboxed body itself will use.
//!
//! Both prefix-intersecting composition surfaces build a `PrefixSet` and
//! meet on the resolved form: the sandbox-projection fold via
//! [`resolve`](PrefixSet::resolve) (it holds a [`Resolver`]), and the
//! `Capabilities` composition meets via
//! [`from_frozen`](PrefixSet::from_frozen) (their prefixes are already
//! frozen, so only canonicalisation is left).  Judging containment on the
//! resolved form — not the surface string — is what stops a symlinked
//! deeper prefix from surviving as authority outside a shallower ceiling.

use super::lex::path_within_str;
use super::resolved::NormalizedPrefix;
use super::resolver::Resolver;
use crate::types::Meet;
use std::path::PathBuf;

/// One prefix held in both forms; see the module note for why the
/// duality is load-bearing rather than redundant.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Prefix {
    resolved: String,
    surface: String,
}

/// A sorted, deduplicated set of [`Prefix`]es.  `Default` is the empty
/// set, the identity for [`union`](PrefixSet::union).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrefixSet(Vec<Prefix>);

impl PrefixSet {
    /// Resolve each prefix through `resolver` into both forms:
    /// [`Resolver::resolve`] applies sigil/`~` expansion and `.`/`..`
    /// normalisation (already done for a [`NormalizedPrefix`], idempotent
    /// here), then [`crate::path::ResolvedPath::canonicalise_lenient`] follows symlinks
    /// across whatever prefix of the path exists.  Accepts either the
    /// grant-side [`NormalizedPrefix`]es or the bare exec-dir strings.
    pub fn resolve<S: AsRef<str>>(resolver: &Resolver, prefixes: &[S]) -> Self {
        let mut set: Vec<Prefix> = prefixes
            .iter()
            .map(|prefix| {
                let rp = resolver.resolve(prefix.as_ref());
                Prefix {
                    resolved: rp.canonicalise_lenient().to_string_lossy().into_owned(),
                    surface: rp.as_path().to_string_lossy().into_owned(),
                }
            })
            .collect();
        normalise(&mut set);
        Self(set)
    }

    /// Build a set from already-frozen prefixes, following each one's
    /// symlinks.  A frozen [`NormalizedPrefix`] (and the frozen exec-dir
    /// strings) is absolute and `.`/`..`-collapsed with every sigil
    /// expanded — exactly the work [`resolve`](Self::resolve) needs a
    /// [`Resolver`] for — so only [`canonicalise_lenient`](super::canon)
    /// remains.  This is the resolver-free door used by the
    /// `Capabilities` composition meets, which hold no `Resolver`; it
    /// lets them judge prefix overlap on the same resolved form the gate
    /// and the projection do.
    pub fn from_frozen(prefixes: &[NormalizedPrefix]) -> Self {
        let mut set: Vec<Prefix> = prefixes
            .iter()
            .map(|prefix| {
                let surface = prefix.as_str().to_string();
                let resolved = super::canon::canonicalise_lenient(&PathBuf::from(&surface))
                    .to_string_lossy()
                    .into_owned();
                Prefix { resolved, surface }
            })
            .collect();
        normalise(&mut set);
        Self(set)
    }

    /// Accumulate two sets without intersecting — the union a deny
    /// region wants, since denies are sticky across layers.
    pub fn union(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        normalise(&mut self.0);
        self
    }

    /// The surface forms, sorted and unique — what a consumer emits.
    /// Minted as [`NormalizedPrefix`]es so the OS-projection `FsPolicy`
    /// carries the same normal form the grant side does.
    pub fn surface(&self) -> Vec<NormalizedPrefix> {
        let mut out: Vec<String> = self.0.iter().map(|p| p.surface.clone()).collect();
        out.sort();
        out.dedup();
        out.into_iter()
            .map(NormalizedPrefix::from_surface)
            .collect()
    }
}

/// Intersect two prefix lists, keeping every element covered by some
/// element of the other list — i.e. the deeper prefix of each
/// overlapping pair survives.  `key` projects each item to the path
/// string overlap is judged on; [`Meet`] always keys on the
/// symlink-resolved form, so a confinement meet can never fall back to
/// lexical (surface-string) overlap.  The result is unsorted and may
/// contain duplicates; the caller applies its own dedup/ordering.
fn meet_prefix_sets_by<T: Clone>(a: &[T], b: &[T], key: impl Fn(&T) -> &str) -> Vec<T> {
    let covered = |x: &T, others: &[T]| others.iter().any(|o| path_within_str(key(x), key(o)));
    a.iter()
        .filter(|x| covered(x, b))
        .cloned()
        .chain(b.iter().filter(|y| covered(y, a)).cloned())
        .collect()
}

/// Intersection: a prefix survives iff some prefix on each side covers
/// it, and the deeper prefix of each overlapping pair is kept.  Overlap
/// is judged on the resolved form via the alias-aware
/// [`meet_prefix_sets_by`], so layers naming one directory through
/// different symlinks intersect correctly.
impl Meet for PrefixSet {
    fn meet(self, other: Self) -> Self {
        let mut out = meet_prefix_sets_by(&self.0, &other.0, |p| p.resolved.as_str());
        normalise(&mut out);
        crate::dbg_trace!(
            "grant-prefix",
            "meet: {:?} ∩ {:?} = {:?}",
            self.0,
            other.0,
            out
        );
        Self(out)
    }
}

/// Sort by resolved form (surface as tiebreak) and drop duplicates —
/// the invariant every `PrefixSet` holds.
fn normalise(set: &mut Vec<Prefix>) {
    set.sort_by(|a, b| {
        a.resolved
            .cmp(&b.resolved)
            .then_with(|| a.surface.cmp(&b.surface))
    });
    set.dedup();
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    fn p(s: &str) -> Prefix {
        Prefix {
            resolved: s.into(),
            surface: s.into(),
        }
    }
    fn set(ps: &[&str]) -> PrefixSet {
        PrefixSet(ps.iter().map(|s| p(s)).collect())
    }
    fn surface(set: &PrefixSet) -> Vec<String> {
        set.surface()
            .into_iter()
            .map(NormalizedPrefix::into_string)
            .collect()
    }

    #[test]
    fn meet_keeps_the_deeper_prefix_of_each_overlapping_pair() {
        // {/a,/b} ∩ {/a/x,/c} admits only /a/x (covered by /a on the left, itself on the right).
        assert_eq!(
            surface(&set(&["/a", "/b"]).meet(set(&["/a/x", "/c"]))),
            vec!["/a/x".to_string()]
        );
    }

    #[test]
    fn meet_is_idempotent() {
        let a = set(&["/a", "/a/b", "/c"]);
        assert_eq!(a.clone().meet(a.clone()), a);
    }

    #[test]
    fn meet_is_commutative() {
        let a = set(&["/a", "/b/c"]);
        let b = set(&["/a/x", "/b"]);
        assert_eq!(a.clone().meet(b.clone()), b.meet(a));
    }

    #[test]
    fn union_accumulates_and_dedups() {
        assert_eq!(
            surface(&set(&["/a", "/b"]).union(set(&["/b", "/c"]))),
            vec!["/a".to_string(), "/b".to_string(), "/c".to_string()]
        );
    }

    /// Security regression.  A deeper grant that *lexically* nests under
    /// a shallower ceiling but resolves — through a symlink — outside it
    /// must not survive the intersection.  If it did, the surviving
    /// prefix would reach the OS sandbox and a spawned child could read
    /// the link's target (`bwrap --bind` follows the source symlink;
    /// Seatbelt matches lexically).  Judging overlap on the *resolved*
    /// form is what closes this — the whole reason `Prefix` carries a
    /// resolved form distinct from its surface form.
    #[cfg(unix)]
    #[test]
    fn symlinked_grant_cannot_escape_a_shallower_ceiling() {
        use crate::path::Resolver;
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("ral-prefix-escape-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok(); // clear any leftover from a crashed run
        let ceiling = root.join("a");
        let outside = root.join("x");
        let escape = ceiling.join("link");
        std::fs::create_dir_all(&ceiling).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, &escape).unwrap();

        let r = Resolver {
            home: "/h".into(),
            cwd: None,
        };
        let base = PrefixSet::resolve(&r, &[ceiling.to_string_lossy().into_owned()]);
        let inner = PrefixSet::resolve(&r, &[escape.to_string_lossy().into_owned()]);

        assert!(
            base.meet(inner).surface().is_empty(),
            "a symlinked grant escaping the ceiling must collapse to the empty (fail-closed) meet"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Positive control: a deeper grant that genuinely nests inside the
    /// ceiling survives.  The resolved-form meet *narrows*; it does not
    /// blanket-deny — so the regression above is catching the symlink,
    /// not just an always-empty intersection.
    #[cfg(unix)]
    #[test]
    fn legitimate_nesting_survives_the_meet() {
        use crate::path::Resolver;

        let root = std::env::temp_dir().join(format!("ral-prefix-nest-{}", std::process::id()));
        let ceiling = root.join("a");
        let sub = ceiling.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let r = Resolver {
            home: "/h".into(),
            cwd: None,
        };
        let base = PrefixSet::resolve(&r, &[ceiling.to_string_lossy().into_owned()]);
        let sub_surface = sub.to_string_lossy().into_owned();
        let inner = PrefixSet::resolve(&r, std::slice::from_ref(&sub_surface));

        assert_eq!(
            surface(&base.meet(inner)),
            vec![sub_surface],
            "a genuinely-nested deeper grant must survive the intersection"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// `PrefixSet::meet` folds over `path_within_str`
    /// ([`super::lex::path_within`]), whose Windows-identity branch —
    /// case-insensitive, separator-unifying comparison — only fires
    /// under a genuine `cfg!(windows)` build (the real platform gate
    /// lives at `path_within`, per its own doc comment).  So, like the
    /// macOS-only alias tests above, this is gated on `cfg(windows)`
    /// rather than run host-independently: a grant on `C:\work` must
    /// still admit `c:/WORK/sub` (same drive, different case and
    /// separator spelling) once folded through the composed meet, not
    /// just through `starts_with_identity` in isolation (pinned
    /// already in `lex`'s own tests).
    #[cfg(windows)]
    #[test]
    fn meet_admits_windows_case_and_separator_variant() {
        let result = set(&[r"C:\work"]).meet(set(&["c:/WORK/sub"]));
        assert!(
            !result.surface().is_empty(),
            "a grant on C:\\work must admit c:/WORK/sub through the composed meet"
        );
    }

    /// Same property, through a `\\?\`-verbatim spelling — what
    /// `std::fs::canonicalize` returns on Windows — so a grant
    /// resolved through canonicalisation still meets a candidate that
    /// was never canonicalized.
    #[cfg(windows)]
    #[test]
    fn meet_admits_windows_verbatim_prefix_variant() {
        let result = set(&[r"C:\work"]).meet(set(&[r"\\?\C:\work\sub"]));
        assert!(
            !result.surface().is_empty(),
            "a grant on C:\\work must admit \\\\?\\C:\\work\\sub through the composed meet"
        );
    }
}
