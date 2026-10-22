//! Path identity: which spellings of a name are one name, and how a rule
//! holds a subject — a position containing another ([`RealPath::contains`]),
//! and an exclusion's holds matching an access by position or by the
//! identities its walk passed ([`Excluded::matches`]).
//!
//! The firmlink table lives in [`super::canon`], so the matcher and the
//! renderers can never see different aliases.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use icu_casemap::CaseMapper;
use icu_locale_core::LanguageIdentifier;
use icu_normalizer::DecomposingNormalizerBorrowed;

use super::PathRules;
use super::lex::parse_bytes;
use super::walk::{EntryName, Excluded, Hold, Listed, ObjectId, RealPath, Step, Trace};

/// `p` plus any alternate spelling the host treats as the same file
/// (`/tmp/foo` ↔ `/private/tmp/foo` on macOS; just `[p]` elsewhere), so a
/// position recorded in one spelling still holds a subject in the other.
fn path_aliases(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    out.extend(super::canon::firmlink_toggle(p));
    out
}

/// Which spellings of a name count as one name.  A positive reading is under
/// [`Stored`](Self::Stored), a negative one under [`Collision`](Self::Collision).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    /// The name as stored, with Windows heads and separators normalised.
    Stored,
    /// Every name some filesystem takes for the same: `collision_key` per
    /// component.
    Collision,
}

/// How an exclusion matched, in citation order: the least is cited.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MatchedBy {
    Name,
    FoldedName,
    Identity,
}

/// What a rule is asked about: an access, by its transcript; a listing's
/// record of an entry, for a name shown; a position, for a scope's own
/// subject and a listing entry never seen; everywhere, the universal
/// allowance's own subject.  `Listed` is not `Access`: a door that mints
/// authority takes a `Located`, so a listing's record can reach `admits` and
/// never `judge`.
#[derive(Clone, Copy)]
pub(crate) enum Subject<'a> {
    Access(&'a Trace),
    Listed(&'a Listed<'a>),
    Position(&'a RealPath),
    Everywhere,
}

impl<'a> Subject<'a> {
    /// The position the subject is at; `None` for everywhere, which is none.
    pub(crate) fn position(self) -> Option<Cow<'a, RealPath>> {
        match self {
            Self::Access(trace) => Some(Cow::Borrowed(trace.real())),
            Self::Listed(listed) => Some(Cow::Owned(listed.real())),
            Self::Position(position) => Some(Cow::Borrowed(position)),
            Self::Everywhere => None,
        }
    }

    /// Whether the subject is at `position` or beneath it, names as stored:
    /// the positive reading, never true of everywhere.
    pub(crate) fn beneath(self, position: &RealPath) -> bool {
        self.position().is_some_and(|at| position.contains(&at))
    }
}

/// How far an exclusion reaches: the subject at or beneath it (fs, an exec
/// directory), or exactly it (an exec file).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Depth {
    Within,
    Exact,
}

impl RealPath {
    /// Whether `other` is this position or lies beneath it, component by
    /// component, names as stored: the positive reading.
    pub fn contains(&self, other: &Self) -> bool {
        path_within(other.as_path(), self.as_path(), Identity::Stored)
    }

    /// The same under `collision_key`, every spelling some filesystem takes
    /// for one name: the negative reading by position alone, for a refusal
    /// that must catch every spelling of a protected place and holds nothing.
    pub fn contains_folded(&self, other: &Self) -> bool {
        path_within(other.as_path(), self.as_path(), Identity::Collision)
    }

    /// How this exclusion holds `path`, if it does: `Name` as stored,
    /// `FoldedName` only under another spelling of a name.  `Within` holds
    /// this position and everything beneath it, `Exact` this position alone.
    fn matched_by(&self, path: &Path, depth: Depth) -> Option<MatchedBy> {
        let holds = |identity| {
            path_within(path, self.as_path(), identity)
                && (depth == Depth::Within || path_within(self.as_path(), path, identity))
        };
        if holds(Identity::Stored) {
            Some(MatchedBy::Name)
        } else {
            holds(Identity::Collision).then_some(MatchedBy::FoldedName)
        }
    }
}

/// What an access passed: the identity of every object it opened, the root
/// first, and the names it spelled below the root, one per step and then the
/// tail.  The names past the `j`th object are `names[j..]`.
struct Passed<'a> {
    ids: Vec<ObjectId>,
    names: Vec<&'a EntryName>,
}

impl<'a> Passed<'a> {
    fn of(trace: &'a Trace) -> Self {
        let steps = trace.steps();
        Self {
            ids: std::iter::once(trace.root_id())
                .chain(steps.iter().map(Step::id))
                .collect(),
            names: steps.iter().map(Step::name).chain(trace.tail()).collect(),
        }
    }

    /// A listing's record: the parent's trace plus its one step.
    fn listed(listed: &'a Listed<'a>) -> Self {
        let mut passed = Self::of(listed.parent());
        passed.ids.push(listed.step().id());
        passed.names.push(listed.step().name());
        passed
    }
}

/// Whether `rule`'s names are a prefix of `access`'s under `collision_key`
/// (the whole of them under `Exact`).
fn names_fit(rule: &[&EntryName], access: &[&EntryName], depth: Depth) -> bool {
    fn key<'a>(name: &'a &EntryName) -> Cow<'a, [u8]> {
        encoded_collision_key(name.as_os_str().as_encoded_bytes())
    }
    let long_enough = match depth {
        Depth::Within => rule.len() <= access.len(),
        Depth::Exact => rule.len() == access.len(),
    };
    long_enough && rule.iter().zip(access).all(|(r, a)| key(r) == key(a))
}

impl Hold {
    /// How this hold covers `subject`: by position, else by identity.  For
    /// each step `k` of the rule and each object `j` the access passed (the
    /// root counted) with the same identity, the rule's names past `k` fit
    /// the access's past `j`.  The rule's root is never matched: every walk
    /// shares it, and a match there is the position.
    fn matches(&self, subject: Subject<'_>, depth: Depth) -> Option<MatchedBy> {
        let position = subject.position()?;
        let by_position = self.trace().real().matched_by(position.as_path(), depth);
        by_position.or_else(|| {
            let access = match subject {
                Subject::Access(trace) => Passed::of(trace),
                Subject::Listed(listed) => Passed::listed(listed),
                Subject::Position(_) | Subject::Everywhere => return None,
            };
            let rule = Passed::of(self.trace());
            let reaches = rule.ids.iter().enumerate().skip(1).any(|(k, id)| {
                let at = |j: usize| names_fit(&rule.names[k..], &access.names[j..], depth);
                (access.ids.iter().enumerate()).any(|(j, other)| other == id && at(j))
            });
            reaches.then_some(MatchedBy::Identity)
        })
    }
}

impl Excluded {
    /// Whether this exclusion covers `subject`, by either of its holds: the
    /// least [`MatchedBy`] of the two.  `Within`: the subject at or beneath
    /// the rule; `Exact`: the rule's position itself, or the identity reading
    /// with the rule's remaining names matching the whole rest of the
    /// subject's.  A `Position` is read positionally alone; `Everywhere` is
    /// excluded by nothing.
    pub(crate) fn matches(&self, subject: Subject<'_>, depth: Depth) -> Option<MatchedBy> {
        self.holds()
            .filter_map(|hold| hold.matches(subject, depth))
            .min()
    }
}

/// The name a filesystem may take `name` for: compatibility caseless
/// matching (Unicode D147) of the uppercase image of its compatibility
/// decomposition.  A name that is not UTF-8 is its own key; no filesystem
/// folds one.
///
/// Coarser than or equal to every identity in play (case-insensitive APFS
/// and Seatbelt, NTFS `$UpCase`, Linux casefold, ZFS under every
/// `casesensitivity` and `normalization`) and so fail-closed where a volume
/// is finer.  NFKD comes first: `𝚤` decomposes to ı, which ZFS upcases to I,
/// yet has no uppercase of its own.
///
/// Seen by the macOS host's stored-name rule alone, which asks whether the
/// name a directory answers is a spelling of the name asked.
#[cfg(any(target_os = "macos", test))]
pub(super) fn collision_key(name: &OsStr) -> Cow<'_, OsStr> {
    #[allow(
        clippy::disallowed_methods,
        reason = "native: a name that is not UTF-8 is its own key"
    )]
    let Some(s) = name.to_str() else {
        return Cow::Borrowed(name);
    };
    Cow::Owned(collision_fold(s).into())
}

/// `collision_key` of a name as its host encodes it
/// ([`OsStr::as_encoded_bytes`]): a name that is not UTF-8 is its own key
/// here too, byte for byte, so no two such names merge as their
/// replacement-character decodings would.
fn encoded_collision_key(name: &[u8]) -> Cow<'_, [u8]> {
    std::str::from_utf8(name).map_or(Cow::Borrowed(name), |s| {
        Cow::Owned(collision_fold(s).into_bytes())
    })
}

/// `collision_key` of a name that is UTF-8.
pub(super) fn collision_fold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let nfd = DecomposingNormalizerBorrowed::new_nfd();
    let nfkd = DecomposingNormalizerBorrowed::new_nfkd();
    let case = CaseMapper::new();
    let upper = case
        .uppercase_to_string(&nfkd.normalize(s), &LanguageIdentifier::UNKNOWN)
        .into_owned();
    let once = nfkd
        .normalize(&case.fold_string(&nfd.normalize(&upper)))
        .into_owned();
    nfkd.normalize(&case.fold_string(&once)).into_owned()
}

/// True iff some alias of `path` starts with some alias of `prefix`: `path`
/// lies inside `prefix` modulo firmlinks and `identity`, on Windows with
/// heads and separators normalised under either.
///
/// Every authority judgment, fs and exec, decides containment through it.
///
/// Private: the kernel is form-blind, and under which identity a containment
/// question is asked is settled by the methods beside it —
/// [`RealPath::contains`] and [`contains_folded`](RealPath::contains_folded)
/// for positions, [`Excluded::matches`] for exclusions — and never by a
/// caller holding two bare paths.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: the containment kernel's byte-wise prefix test"
)]
fn path_within(path: &Path, prefix: &Path, identity: Identity) -> bool {
    let rules = PathRules::HOST;
    let ps = path_aliases(path);
    let qs = path_aliases(prefix);
    // Every arm compares names as the host encodes them, never their
    // `to_string_lossy` forms: two distinct names that are not Unicode (bytes
    // off Windows, an unpaired surrogate on it) can decode to one
    // replacement-character string, and the matcher would call them one.
    ps.iter().any(|p| {
        qs.iter().any(|q| match identity {
            Identity::Stored if rules == PathRules::Windows => {
                windows_stored_components(p.as_os_str())
                    .starts_with(&windows_stored_components(q.as_os_str()))
            }
            Identity::Stored => p.starts_with(q),
            Identity::Collision => starts_with_collision(p, q, rules),
        })
    })
}

/// [`path_within`] on strings, names as stored, for the renderers that hold the
/// kernel's names.
#[cfg(target_os = "linux")]
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: lifts both strings for `path_within`; nothing is resolved"
)]
pub(super) fn path_within_str(path: &str, prefix: &str) -> bool {
    path_within(Path::new(path), Path::new(prefix), Identity::Stored)
}

/// Component-wise prefix test under `collision_key`: split as
/// [`starts_with_identity`] splits under Windows rules, by
/// [`Path::components`] otherwise, then compared key by key, so no fold
/// crosses a component boundary (`/w/secretsx` is not within `/w/Secrets`).
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: the containment kernel's component-wise key"
)]
fn starts_with_collision(path: &Path, prefix: &Path, rules: PathRules) -> bool {
    let keys = |p: &Path| -> Vec<Vec<u8>> {
        match rules {
            PathRules::Windows => windows_components(p.as_os_str().as_encoded_bytes())
                .map(|c| encoded_collision_key(c).into_owned())
                .collect(),
            PathRules::Posix => p
                .components()
                .map(|c| encoded_collision_key(c.as_os_str().as_encoded_bytes()).into_owned())
                .collect(),
        }
    };
    let (path, prefix) = (keys(path), keys(prefix));
    path.len() >= prefix.len() && path[..prefix.len()] == prefix[..]
}

/// Component-wise prefix test: byte-exact under POSIX rules (via
/// [`Path::starts_with`], so `/tmp` never matches `/tmpx`), and under Windows
/// rules by Windows path identity: `/` ≡ `\`, ASCII-case-insensitive
/// components, a `\\?\`-verbatim prefix equivalent to its plain spelling.
///
/// A display identity, not an authority one: the prompt's home-strip asks
/// it whether to fold a path to `~`.  Grant containment, [`path_within`],
/// never asks it: an allow there keeps case ([`Identity::Stored`]) and a
/// deny folds by `collision_key`.
///
/// String logic rather than `std::path::Path`, whose separator and prefix
/// parsing are fixed at compile time to the build target.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: POSIX identity is `Path::starts_with` itself, byte-exact per component"
)]
pub(crate) fn starts_with_identity(path: &str, prefix: &str, rules: PathRules) -> bool {
    if rules == PathRules::Posix {
        return Path::new(path).starts_with(Path::new(prefix));
    }
    let path = windows_identity_components(path);
    let prefix = windows_identity_components(prefix);
    path.len() >= prefix.len() && path[..prefix.len()] == prefix[..]
}

/// A path string as lower-cased components under Windows path identity: a
/// verbatim prefix stripped, a verbatim UNC head folded to `\server\share`,
/// `/` and `\` alike as separators.
///
/// The verbatim prefix is recognised in either slash spelling, though real
/// Windows honours only `\\?\` at the `CreateFileW` boundary: this is an
/// internal normalisation, and folding `//?/C:/work` differently from
/// `\\?\C:\work` would leave a deny that a differently-spelled access slips
/// past.  The case fold is ASCII-only, matching
/// `which`'s `name_key_on` so paths and command names fold alike, and
/// erring below the real NTFS `$UpCase` table rather than above it: missing
/// non-ASCII folds sooner than claiming equivalences the driver would refuse.
pub(crate) fn windows_identity_components(p: &str) -> Vec<Vec<u8>> {
    windows_components(p.as_bytes())
        .map(<[u8]>::to_ascii_lowercase)
        .collect()
}

/// A native path as one key under which every spelling some filesystem may
/// take for the same object collides: split as
/// [`windows_identity_components`] splits, each component through
/// `collision_key`.  For a caller that must name a kernel object by a path
/// and errs safe by merging, never by splitting: a lock.
///
/// Coarser than NTFS, which upcases each UTF-16 unit by its volume's
/// `$UpCase` table, one simple (single-character) uppercase mapping per
/// unit: every pair of names that table equates shares a compatibility
/// caseless key, so two spellings of one NTFS name never part.  The excess,
/// `ß` with `SS` or `ﬁ` with `fi`, merges names NTFS keeps apart, which a
/// lock pays for in a wait.  Not for a witness or a capability, which would
/// fail open by merging.
///
/// Defined on absolute paths alone: the key keeps no root separators, so a
/// relative `srv\sh\x` keys as `\\srv\sh\x` does, and `x` as `\x`.
///
/// Bytes in the host's encoding ([`OsStr::as_encoded_bytes`]), not a
/// `String`: a component that is not Unicode keys as itself, so two such
/// names never merge through a replacement character.
#[cfg(any(windows, test))]
pub(crate) fn windows_collision_key(p: &OsStr) -> Vec<u8> {
    windows_components(p.as_encoded_bytes())
        .map(encoded_collision_key)
        .collect::<Vec<_>>()
        .join(&b'\\')
}

/// Canonical names keep their case; a drive letter is not a directory entry.
///
/// Each component in the host's encoding ([`OsStr::as_encoded_bytes`]), so
/// the identity is lossless: a name holding an unpaired surrogate is not the
/// name holding U+FFFD that its lossy decoding would be.
pub(crate) fn windows_stored_components(p: &OsStr) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = windows_components(p.as_encoded_bytes())
        .map(<[u8]>::to_vec)
        .collect();
    if let Some(drive) = parts.first_mut()
        && drive.ends_with(b":")
    {
        drive.make_ascii_lowercase();
    }
    parts
}

/// A path's components under Windows path identity, past its
/// [head](windows_head), in the encoding it came in: the bytes of a `str`, or
/// of an [`OsStr`] as the host encodes it.  The one split
/// [`windows_identity_components`], [`windows_stored_components`] and
/// [`starts_with_collision`] share.
///
/// Lossless on either encoding: a separator is ASCII, and an ASCII byte in
/// either stands for that character alone; a head is ASCII, so slicing at its
/// boundary preserves the encoding of the rest.
fn windows_components(p: &[u8]) -> impl Iterator<Item = &[u8]> {
    parse_bytes(p, PathRules::Windows)
        .without_verbatim_prefix()
        .split(|&b| PathRules::Windows.is_separator(char::from(b)))
        .filter(|c| !c.is_empty())
}

/// A path string past its Windows head: a verbatim prefix stripped, and a
/// verbatim UNC head's `UNC` with it, so `\\?\UNC\server\share` splits as
/// `\\server\share` does.  The step [`windows_components`] takes before it
/// splits, shared with the prompt's home-strip (`super::tilde`), which needs
/// the head fold without the case fold.  One copy, because a head the two
/// folded differently would silently unfold a `~`.
pub(crate) fn windows_head(p: &str) -> &str {
    let rest = parse_bytes(p.as_bytes(), PathRules::Windows).without_verbatim_prefix();
    // SAFETY: `without_verbatim_prefix` slices `p` at an ASCII prefix boundary,
    // which cannot land inside a multi-byte UTF-8 sequence.
    unsafe { std::str::from_utf8_unchecked(rest) }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures compare literal paths"
)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    /// Every comparison and key, of `raw` and of `lossy`, a name that is not
    /// Unicode and the replacement-character spelling it decodes to: none
    /// may call them one.
    fn assert_lossless(raw: &Path, lossy: &Path) {
        assert_eq!(
            raw.to_string_lossy(),
            lossy.to_string_lossy(),
            "the two must decode alike, or this proves nothing"
        );
        for identity in [Identity::Stored, Identity::Collision] {
            assert!(!path_within(raw, lossy, identity), "{identity:?}");
            assert!(!path_within(lossy, raw, identity), "{identity:?}");
        }
        for (p, q) in [(raw, lossy), (lossy, raw)] {
            assert!(!starts_with_collision(p, q, PathRules::Windows));
            assert!(!starts_with_collision(p, q, PathRules::Posix));
        }
        assert_ne!(
            windows_stored_components(raw.as_os_str()),
            windows_stored_components(lossy.as_os_str())
        );
        assert_ne!(
            windows_collision_key(raw.as_os_str()),
            windows_collision_key(lossy.as_os_str())
        );
    }

    /// A byte that is not UTF-8 is not U+FFFD, under either identity and
    /// either split.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_byte_is_not_its_replacement_character() {
        use std::os::unix::ffi::OsStrExt;
        assert_lossless(
            Path::new(OsStr::from_bytes(b"/opt/\xFFsecret")),
            Path::new("/opt/\u{FFFD}secret"),
        );
    }

    /// An unpaired surrogate is not U+FFFD, under either identity and
    /// either split.
    #[cfg(windows)]
    #[test]
    fn an_unpaired_surrogate_is_not_its_replacement_character() {
        use std::os::windows::ffi::OsStringExt;
        let wide: Vec<u16> = r"C:\opt\"
            .encode_utf16()
            .chain([0xD800])
            .chain("secret".encode_utf16())
            .collect();
        assert_lossless(
            &PathBuf::from(std::ffi::OsString::from_wide(&wide)),
            Path::new("C:\\opt\\\u{FFFD}secret"),
        );
    }

    #[test]
    fn spellings_of_one_windows_path_share_a_collision_key() {
        let key = windows_collision_key(os(r"C:\Tmp\Foo"));
        for spelling in [r"c:\tmp\foo", r"\\?\C:\TMP\foo", "c:/tmp/foo/"] {
            assert_eq!(windows_collision_key(os(spelling)), key, "{spelling}");
        }
        assert_eq!(
            windows_collision_key(os(r"\\?\UNC\Srv\Sh\x")),
            windows_collision_key(os(r"\\srv\sh\X"))
        );
        assert_ne!(
            windows_collision_key(os(r"C:\tmp\foo")),
            windows_collision_key(os(r"C:\tmp\fo"))
        );
    }

    /// NTFS upcases beyond ASCII, so a lock keyed by a path must too: the
    /// two spellings below are one NTFS name.
    #[test]
    fn a_collision_key_folds_beyond_ascii() {
        for (a, b) in [
            (r"C:\Ä\state", r"C:\ä\state"),
            (r"C:\Σ\x", r"C:\σ\x"),
            (r"C:\work\ÉCLAIR", r"c:\WORK\éclair"),
        ] {
            assert_eq!(
                windows_collision_key(os(a)),
                windows_collision_key(os(b)),
                "{a} {b}"
            );
        }
    }

    /// Each row: a path and the alias it gains on macOS (none elsewhere).
    #[test]
    fn aliases_cross_the_firmlinks_on_macos_only() {
        let rows = [
            ("/tmpx/foo", None),
            ("/tmp/foo", Some("/private/tmp/foo")),
            ("/private/tmp/foo", Some("/tmp/foo")),
            ("/var/folders/xy/abc", Some("/private/var/folders/xy/abc")),
            ("/tmp", Some("/private/tmp")),
        ];
        for (path, alias) in rows {
            let got = path_aliases(Path::new(path));
            assert!(got.contains(&pb(path)), "{path}");
            match alias.filter(|_| cfg!(target_os = "macos")) {
                Some(alias) => assert!(got.contains(&pb(alias)), "{path}"),
                None => assert_eq!(got, vec![pb(path)], "{path}"),
            }
        }
    }

    #[test]
    fn path_within_is_component_wise_modulo_aliases() {
        let rows = [
            ("/a/b", "/a/b", true),
            ("/a/b/c", "/a/b", true),
            ("/a/b", "/a/c", false),
            ("/a", "/a/b", false),
            ("/tmpx", "/tmp", false),
            #[cfg(target_os = "macos")]
            ("/tmp/foo", "/private/tmp", true),
            #[cfg(target_os = "macos")]
            ("/private/tmp/foo", "/tmp", true),
        ];
        for (path, dir, want) in rows {
            assert_eq!(
                path_within(Path::new(path), Path::new(dir), Identity::Stored),
                want,
                "{path} within {dir}"
            );
        }
    }

    /// Security regression: two distinct non-UTF-8 byte sequences can decode
    /// to the same U+FFFD-substituted string, so comparing lossy forms would
    /// let an unrelated path match a grant prefix.
    #[cfg(unix)]
    #[test]
    fn path_within_does_not_collide_distinct_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // `/opt` (unlike `/tmp`, `/var`, `/etc`) is no firmlink source, so
        // these never reach `firmlink_toggle`: what is under test is
        // `path_within`'s comparison, not the aliasing.
        let prefix_bytes: &[u8] = b"/opt/\xFFsecret";
        let candidate_bytes: &[u8] = b"/opt/\xFEsecret";
        // Sanity: the two invalid bytes really do lossy-collide, or this
        // test proves nothing.
        assert_eq!(
            Path::new(OsStr::from_bytes(prefix_bytes)).to_string_lossy(),
            Path::new(OsStr::from_bytes(candidate_bytes)).to_string_lossy(),
        );

        let prefix = Path::new(OsStr::from_bytes(prefix_bytes));
        let candidate_path = PathBuf::from(OsStr::from_bytes(candidate_bytes)).join("file");
        assert!(
            !path_within(&candidate_path, prefix, Identity::Stored),
            "distinct non-UTF-8 paths must not collide via lossy string comparison"
        );
    }

    /// T1: the key merges every pair some filesystem merges, and nothing
    /// finer.
    #[test]
    fn collision_key_merges_what_some_filesystem_merges() {
        let key = |s: &str| collision_key(OsStr::new(s)).into_owned();
        for (a, b) in [
            ("Secrets", "secrets"),
            ("caf\u{e9}", "cafe\u{301}"),
            ("stra\u{df}e", "strasse"),
            ("\u{df}", "\u{1e9e}"),
            ("\u{fb01}le", "file"),
            ("\u{212a}ey", "key"),
            ("\u{3c3}", "\u{3c2}"),
            ("\u{130}", "i\u{307}"),
            ("f\u{131}le", "FILE"),
            ("\u{1d6a4}", "\u{131}"),
            ("\u{ff21}", "a"),
        ] {
            assert_eq!(key(a), key(b), "{a:?} and {b:?} must collide");
        }
        for (a, b) in [("abc", "abd"), ("secret", "secrets")] {
            assert_ne!(key(a), key(b), "{a:?} and {b:?} must stay distinct");
        }
    }

    /// T1: no filesystem folds a name that is not UTF-8, so the key is the
    /// name, byte for byte.
    #[cfg(unix)]
    #[test]
    fn collision_key_leaves_a_non_utf8_name_its_own() {
        use std::os::unix::ffi::OsStrExt;
        let (ff, fe) = (OsStr::from_bytes(b"\xFF"), OsStr::from_bytes(b"\xFE"));
        assert_eq!(collision_key(ff), ff);
        assert_ne!(collision_key(ff), collision_key(fe));
    }

    /// T2: a deny's relation folds within a component and never across one;
    /// an allow's does not fold at all.
    #[test]
    fn collision_folds_components_and_stored_does_not() {
        let within = |p: &str, q: &str, identity| path_within(Path::new(p), Path::new(q), identity);
        assert!(within("/w/secrets/t", "/w/Secrets", Identity::Collision));
        assert!(within("/w/SECRETS", "/w/Secrets", Identity::Collision));
        assert!(!within("/w/secretsx", "/w/Secrets", Identity::Collision));
        assert!(!within("/w/other/t", "/w/Secrets", Identity::Collision));
        assert!(!within("/w/secrets/t", "/w/Secrets", Identity::Stored));
    }

    /// T2 under the Windows split, pinned on every host: heads and
    /// separators fold as [`starts_with_identity`] folds them, and non-ASCII
    /// case, which the stored identity misses, collides.
    #[test]
    fn windows_collision_folds_non_ascii_case() {
        let within = |p: &str, q: &str| {
            starts_with_collision(Path::new(p), Path::new(q), PathRules::Windows)
        };
        assert!(within(r"\\?\C:\w\éclair\x", "c:/W/ÉCLAIR"));
        assert!(!within(r"C:\w\eclair", r"C:\w\Éclair"));
        assert!(!starts_with_identity(
            r"C:\w\éclair",
            r"C:\w\Éclair",
            PathRules::Windows
        ));
    }

    // The Windows rules below pass `PathRules::Windows` directly rather than
    // hiding behind `cfg(windows)`, so they are pinned on every host.
    const W: PathRules = PathRules::Windows;

    #[test]
    fn windows_stored_names_distinguish_case_except_in_the_drive() {
        assert_eq!(
            windows_stored_components(os(r"\\?\C:\Work\file")),
            windows_stored_components(os("c:/Work/file"))
        );
        assert_ne!(
            windows_stored_components(os(r"C:\Work")),
            windows_stored_components(os(r"C:\work"))
        );
    }

    /// `(path, prefix, rules, expected)`.  The forward-slash verbatim rows
    /// matter: a `//?/`-spelled access must not bypass a `\\?\`- or
    /// plain-spelled deny, so `/` and `\` are interchangeable throughout; a
    /// `UNC` head folds only after a verbatim prefix.
    #[test]
    fn starts_with_identity_table() {
        const P: PathRules = PathRules::Posix;
        let rows = [
            (r"C:\WORK\sub", r"c:\work", W, true),
            (r"c:\Work\Sub", r"C:\WORK", W, true),
            (r"c:/work/sub", r"C:\work", W, true),
            (r"C:\work\sub", "c:/work", W, true),
            (r"\\?\C:\work\sub", r"C:\work", W, true),
            (r"C:\work\sub", r"\\?\C:\work", W, true),
            (r"\\?\C:\work\sub", r"\\?\c:\WORK", W, true),
            (r"\\?\UNC\server\share\sub", r"\\server\share", W, true),
            (r"//?/C:/work/sub", r"C:\work", W, true),
            (r"C:\work\sub", "//?/c:/work", W, true),
            (r"//?/C:/work/sub", r"\\?\c:\WORK", W, true),
            (r"//?\C:\work/sub", r"C:\work", W, true),
            ("//?/UNC/server/share/sub", r"\\server\share", W, true),
            (r"\\?\unc\srv\sh\x", r"\\srv\sh", W, true),
            (r"UNC\srv\x", r"\\srv", W, false),
            (r"C:\workshop", r"C:\work", W, false),
            (r"D:\work\sub", r"C:\work", W, false),
            (r"C:\WORK", r"C:\work", P, false),
        ];
        for (path, prefix, rules, want) in rows {
            assert_eq!(
                starts_with_identity(path, prefix, rules),
                want,
                "{path} under {prefix} ({rules:?})"
            );
        }
    }
}
