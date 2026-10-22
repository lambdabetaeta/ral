//! The freeze: the one place a grant's [`Place`]s and [`Selector`]s become
//! [`FrozenPath`]s, against one anchor, [`FreezeCtx`].
//!
//! A grant names no host's home, XDG layout or working directory: a place is
//! a tag and a subpath, `absolute /usr`, `cwd src`, `home .ssh`,
//! `tempdir .`, `gitdir .` or `xdg-NAME sub`, and the anchor is read here,
//! once, so a later `chdir` or `TMPDIR` change cannot retroactively widen a
//! grant.  A selector names directories the environment chooses, and is
//! exec-only: its expansion is held under no `home`, but a toolchain
//! override or `$PATH` entry may not name the world.  XDG uses the Linux
//! defaults on every platform, macOS included ([`crate::host`]).
//!
//! An `absolute` payload is classified on the raw string, before any folding,
//! host first, so that one profile loads on both platforms: live if absolute
//! under this host's rules, dead (a grant naming no path here) if absolute
//! only under the other platform's, refused if under neither.  `/x` is
//! POSIX-absolute and names no drive, `C:\x` and `\\srv\sh\x` are
//! Windows-absolute, and a well-formed `//srv/sh/x` is both, so live on both.
//! A UNC root, plain or verbatim (`\\?\UNC\srv\sh`), needs a server and a
//! share, neither `.` nor `..`; a bare `\\` or `//` is none, and no `\\?\`
//! or `\\.\` path is read as anything but a drive path or such a root.  A
//! head of two mixed separators, `/\x` or `\/srv/sh`, is refused on every
//! host: Windows reads it as a UNC or device head, POSIX as something else.
//! A subpath is judged lexically, so that a profile fails alike everywhere.
//! `~` and `xdg:NAME` are expanded by the resolver at run time, never in a
//! grant, and are refused under every tag.
//!
//! Two anchors read a source the session does not own, and each guards it in
//! the terms of what would otherwise author the grant.  An `xdg-*` grant must
//! land under `home`, not merely under its XDG base directory (`cwd`, `home`,
//! `tempdir` and `gitdir` permit `..` to climb past their anchor, whereas
//! `xdg-*` checks the resolved path against `home`): an `XDG_*_HOME` set to
//! `/etc`, or a `..` subpath escaping `home`, would otherwise widen an
//! `xdg-data` grant to `/etc`.  And a `.git` *file* names its git directory in
//! the working tree's own words, so a `gitdir` that followed the pointer alone
//! would be authored by whoever wrote the tree, and the place is granted for
//! read *and* write.  Following it therefore asks the other direction too: the
//! git directory must name this working tree back, as a linked worktree's
//! `gitdir` file does and as `core.worktree` does for a repository split off
//! with `--separate-git-dir`.  A pointer no git directory claims is a
//! [`PolicyError`], never a wider grant.  `tempdir` alone trusts its source as
//! given, `TMPDIR` being the launching user's to set.

use super::toolchains::{self, Toolchain};
use crate::capability::{GrantPath, Polar, Ungoverned};
use crate::path::git::{BadPointer, Pointer, discover_git_dir};
use crate::path::identity::windows_identity_components;
use crate::path::lex::relative_suffix;
use crate::path::sigil::SigilHead;
use crate::path::{
    CwdPath, FrozenPath, GrantRefusal, LeafMode, LexicalPath, PathRules, RealPath, SubpathRefusal,
    Trace, XdgKind, classify_subpath, grant_eligibility, resolve_xdg_in, shown, walk, walk_lenient,
};
use crate::syntax::quote::quote_word;
use crate::text::code;
use crate::types::{Break, EnvVars, Error, Shell};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::Path;
use std::str::FromStr;
use strum::VariantArray;

/// One path a grant names: a tag and a subpath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Absolute(String),
    Cwd(String),
    Home(String),
    Tempdir(String),
    Gitdir(String),
    Xdg(XdgKind, String),
}

/// The tag half of a [`Place`]: what its subpath is anchored to.  The one
/// declaration of the place tags, which the decoder reads and the checker
/// mints its rows from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Absolute,
    Cwd,
    Home,
    Tempdir,
    Gitdir,
    Xdg(XdgKind),
}

impl Anchor {
    /// Every anchor, in the order a grant lists them.
    pub fn all() -> impl Iterator<Item = Self> {
        [
            Self::Absolute,
            Self::Cwd,
            Self::Home,
            Self::Tempdir,
            Self::Gitdir,
        ]
        .into_iter()
        .chain(XdgKind::VARIANTS.iter().map(|&k| Self::Xdg(k)))
    }

    /// The tag a grant spells this anchor with.
    pub fn tag(self) -> Cow<'static, str> {
        match self {
            Self::Absolute => "absolute".into(),
            Self::Cwd => "cwd".into(),
            Self::Home => "home".into(),
            Self::Tempdir => "tempdir".into(),
            Self::Gitdir => "gitdir".into(),
            Self::Xdg(kind) => format!("xdg-{}", <&str>::from(kind)).into(),
        }
    }

    pub fn of_tag(tag: &str) -> Option<Self> {
        Self::all().find(|a| a.tag() == tag)
    }

    pub fn place(self, sub: String) -> Place {
        match self {
            Self::Absolute => Place::Absolute(sub),
            Self::Cwd => Place::Cwd(sub),
            Self::Home => Place::Home(sub),
            Self::Tempdir => Place::Tempdir(sub),
            Self::Gitdir => Place::Gitdir(sub),
            Self::Xdg(kind) => Place::Xdg(kind, sub),
        }
    }
}

impl Place {
    pub fn anchor(&self) -> Anchor {
        match self {
            Self::Absolute(_) => Anchor::Absolute,
            Self::Cwd(_) => Anchor::Cwd,
            Self::Home(_) => Anchor::Home,
            Self::Tempdir(_) => Anchor::Tempdir,
            Self::Gitdir(_) => Anchor::Gitdir,
            Self::Xdg(kind, _) => Anchor::Xdg(*kind),
        }
    }

    /// The tag a grant spells this place with.
    pub fn tag(&self) -> Cow<'static, str> {
        self.anchor().tag()
    }

    pub fn payload(&self) -> &str {
        match self {
            Self::Absolute(s)
            | Self::Cwd(s)
            | Self::Home(s)
            | Self::Tempdir(s)
            | Self::Gitdir(s)
            | Self::Xdg(_, s) => s,
        }
    }
}

/// A set of directories the environment chooses, valid only under `dir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// Every absolute directory in the host's `$PATH`.
    Path,
    /// A package manager's, or the platform's, directories ([`Toolchain`]).
    Toolchain(Toolchain),
}

impl Selector {
    /// Every selector's name, as a grant spells it.
    pub fn names() -> impl Iterator<Item = &'static str> {
        std::iter::once("path").chain(Toolchain::VARIANTS.iter().map(|&t| <&str>::from(t)))
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Toolchain(t) => t.into(),
        }
    }
}

impl FromStr for Selector {
    type Err = strum::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "path" => Ok(Self::Path),
            _ => s.parse().map(Self::Toolchain),
        }
    }
}

/// A capability-policy decode/freeze failure.
///
/// The capability decoder and the freeze answer a malformed grant with a
/// "no", never a process exit; having no `Escape` arm is how the type checker
/// holds them to it.  A `Break` is minted only at the boundary that needs one.
#[derive(Debug, Clone)]
pub struct PolicyError {
    pub message: String,
    pub hint: Option<String>,
}

impl PolicyError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// A spelling whose walk failed for a reason that is no absent or
    /// unenterable tail.
    pub(crate) fn unwalkable(written: &FrozenPath, error: &std::io::Error) -> Self {
        Self::new(format!(
            "ral cannot resolve '{}', which the grant names: {error}",
            written.shown()
        ))
        .with_hint(
            "A grant is resolved when it loads, so every name it passes through must be \
             one ral can follow: is a link in a loop, or a path component not a directory?",
        )
    }

    /// An exclusion of the filesystem root, which holds no object to name.
    pub(crate) fn root_exclusion(written: &FrozenPath) -> Self {
        Self::new(format!(
            "'{}' is the filesystem root, and excluding the root excludes every path",
            written.shown()
        ))
        .with_hint(
            "To withhold every path, grant none: leave the fs `read` and `write` lists empty.",
        )
    }

    /// A file key naming the filesystem root, a directory, not a program.
    pub(crate) fn root_program(written: &FrozenPath) -> Self {
        Self::new(format!(
            "'{}' is the filesystem root, a directory, so as an exec target it names a binary \
             that cannot exist",
            written.shown()
        ))
        .with_hint(
            "To run every program write `every`; for every program beneath a directory write \
             `dir` before the place.",
        )
    }
}

impl From<PolicyError> for Break {
    fn from(e: PolicyError) -> Self {
        let err = Error::new(e.message);
        Self::Error(match e.hint {
            Some(hint) => err.with_hint(hint),
            None => err,
        })
    }
}

/// The `as_map`/`as_list` coercions raise a bare `Error`: a shape mismatch,
/// never an exit, so the decoder absorbs one directly.
impl From<Error> for PolicyError {
    fn from(e: Error) -> Self {
        match e.hint {
            Some(hint) => Self::new(e.message).with_hint(hint),
            None => Self::new(e.message),
        }
    }
}

/// The `gitdir` place's refusals, in the grant's terms.
impl From<BadPointer> for PolicyError {
    fn from(BadPointer { dot_git, why }: BadPointer) -> Self {
        match why {
            Pointer::Unclaimed(target) => unclaimed_message(&dot_git, &target),
            Pointer::NoGitdirLine => no_pointer_message(&dot_git),
            Pointer::Unreadable(error) => unreadable_message(&dot_git, &error),
        }
    }
}

/// The file at `path`, read under no grant: what git discovery reads at
/// session start, before any grant governs.
fn ungoverned(path: &LexicalPath) -> std::io::Result<String> {
    let file = Ungoverned::read(walk(path, LeafMode::Follow)?);
    std::io::read_to_string(file.read()?)
}

fn unclaimed_message(dot_git: &Path, target: &Path) -> PolicyError {
    PolicyError::new(format!(
        "the .git file at '{dot_git}' points at '{target}', but that directory \
         does not name this working tree back, so {gitdir} will not grant it; \
         a pointer written inside the tree would otherwise choose the grant.",
        dot_git = shown(dot_git),
        gitdir = code("`gitdir"),
        target = shown(target),
    ))
    .with_hint(format!(
        "A linked worktree's git directory holds a `gitdir` file naming this \
             very .git file, and a repository split off with `--separate-git-dir` \
             holds `core.worktree` in its config; neither names this tree.  Was the \
             worktree moved?  `git worktree repair` rewrites both ends.  If the \
             pointer is hand-written, name the git directory explicitly in the \
             policy instead of {}.",
        code("`gitdir")
    ))
}

fn no_pointer_message(dot_git: &Path) -> PolicyError {
    PolicyError::new(format!(
        "the .git entry at '{}' is a file with no `gitdir:` line, so there is \
         no git directory for {} to name.",
        shown(dot_git),
        code("`gitdir"),
    ))
    .with_hint(format!(
        "A worktree's or submodule's `.git` file holds one line, \
             `gitdir: <path>`.  Is this file something else?  If the tree is not a \
             repository, drop {} from the policy or replace it with an \
             explicit path.",
        code("`gitdir")
    ))
}

fn unreadable_message(dot_git: &Path, error: &std::io::Error) -> PolicyError {
    PolicyError::new(format!(
        "the .git file at '{}' cannot be read ({error}), so {} cannot \
         say which git directory the grant covers.",
        shown(dot_git),
        code("`gitdir"),
    ))
    .with_hint(
        "The freeze reads the pointer as the user launching the session.  Check \
         the file's permissions, or replace `gitdir in the policy with an \
         explicit path.",
    )
}

/// A place frozen and walked as polarity `W` walks, or the root spelling,
/// which no host walks: `/` is a grant by polarity alone ([`Polar::root`]).
pub(crate) enum Placed<W: Polar> {
    Root,
    Walked(GrantPath<W>),
}

impl<W: Polar> Placed<W> {
    /// What the polarity makes of it.
    ///
    /// # Errors
    /// What the polarity refuses of the root.
    pub(crate) fn grant(self) -> Result<GrantPath<W::Grant>, PolicyError> {
        match self {
            Self::Root => W::root(FrozenPath::root()),
            Self::Walked(path) => W::grant(path),
        }
    }

    /// The spelling, as written and frozen.
    pub(crate) fn written(&self) -> FrozenPath {
        match self {
            Self::Root => FrozenPath::root(),
            Self::Walked(path) => path.written.clone(),
        }
    }
}

/// The anchor a grant is frozen against, once rather than per access: the
/// [`Resolver`](crate::path::Resolver) pair.
///
/// `home` and `xdg-*` read the environment (overlay, then host), `tempdir`
/// the host's, `gitdir` the disk from `cwd`; no `HOME` refuses a home-relative
/// place rather than resolve it against a stand-in.
#[derive(Debug, Clone)]
pub struct FreezeCtx {
    env: EnvVars,
    cwd: CwdPath,
}

impl FreezeCtx {
    /// The shell's own anchor: the environment its override chain binds and
    /// its logical cwd, so every freeze of one session agrees.
    pub fn of(shell: &Shell) -> Self {
        Self::at(
            shell.context.env_overrides().clone(),
            shell.context.cwd().clone(),
        )
    }

    /// An anchor at `env` and `cwd`, for a caller with no shell.
    pub fn at(env: EnvVars, cwd: CwdPath) -> Self {
        Self { env, cwd }
    }

    /// The directory `cwd` and `gitdir` places anchor to.
    pub fn cwd(&self) -> &CwdPath {
        &self.cwd
    }

    /// The environment a freeze reads, for the `PATH` a bare name is found on.
    pub(crate) fn env(&self) -> &EnvVars {
        &self.env
    }

    fn home(&self) -> Option<String> {
        self.env.home()
    }

    /// The home, walked: where the escape guards ask what lies within it.
    fn home_dir(&self) -> Result<Option<Home>, PolicyError> {
        self.home().as_deref().map(Home::of).transpose()
    }

    /// Freeze one place as an allowance: validate its payload, resolve its
    /// anchor, then fold `.`/`..` and wrap, so every frozen place is in the
    /// normal form the in-process guard matches against.  Resolving here at
    /// load rather than per check is what makes a grant immune to later env
    /// and cwd changes.
    ///
    /// `Ok(None)` is the dead grant: an `absolute` path rooted under the other
    /// platform's rules, which can never match an access here.
    ///
    /// # Errors
    /// As [`walked`](Self::walked).
    pub fn place(&self, place: &Place) -> Result<Option<FrozenPath>, PolicyError> {
        Ok(self.walked::<Trace>(place)?.map(|placed| placed.written()))
    }

    /// Freeze one place and walk it as polarity `W` walks: an allowance's
    /// transcript, an exclusion's holds.  The refusals that judge where a
    /// place lands (`xdg-*` within `home`) judge the walk's own, so what was
    /// judged is what is kept.
    ///
    /// # Errors
    /// A malformed payload, an `XDG_*_HOME` that escapes `HOME`, an unset
    /// `HOME` under a home-relative place, an anchored place that freezes
    /// relative, a `.git` pointer no git directory claims, a spelling whose
    /// walk fails, or an exclusion of the root.
    pub(crate) fn walked<W: Polar>(&self, place: &Place) -> Result<Option<Placed<W>>, PolicyError> {
        let tag = place.tag();
        let sub = |s| subpath(&tag, s);
        let frozen = match place {
            Place::Absolute(raw) => {
                // The root spelling names everywhere on every host and is
                // classified by neither host's rules; every other payload is
                // classified before it is folded, so no foreign or malformed
                // spelling can fold to `/` here.
                if raw == "/" {
                    return Ok(Some(Placed::Root));
                }
                return live_absolute(raw, PathRules::HOST)?
                    .then(|| {
                        freeze(raw)
                            .and_then(GrantPath::<W>::walked)
                            .map(Placed::Walked)
                    })
                    .transpose();
            }
            Place::Cwd(s) => join_sub(self.cwd.as_path(), sub(s)?)?,
            Place::Tempdir(s) => join_sub(std::env::temp_dir(), sub(s)?)?,
            Place::Gitdir(s) => {
                let sub = sub(s)?;
                let cwd = self.cwd.as_path();
                let base = discover_git_dir(cwd, ungoverned)?.unwrap_or_else(|| cwd.to_path_buf());
                join_sub(base, sub)?
            }
            Place::Home(s) => {
                let sub = sub(s)?;
                join_sub(self.home().ok_or_else(home_unknown)?, sub)?
            }
            Place::Xdg(kind, s) => return self.xdg::<W>(*kind, sub(s)?).map(Some),
        };
        GrantPath::<W>::walked(anchored(&tag, frozen)?)
            .map(Placed::Walked)
            .map(Some)
    }

    /// The directories a selector names, each walked as polarity `W` walks.
    /// Selectors read the host, not the anchor, save for the home; they sit
    /// here so that one resolver freezes every grant path.
    ///
    /// # Errors
    /// `path` over a `$PATH` with no absolute entry, or a `$PATH` entry or
    /// toolchain override that names the filesystem root or the home.
    pub(crate) fn selector<W: Polar>(
        &self,
        selector: Selector,
    ) -> Result<Vec<GrantPath<W::Grant>>, PolicyError> {
        let home = self.home_dir()?;
        match selector {
            Selector::Path => path_dirs::<W>(home.as_ref()),
            Selector::Toolchain(t) => {
                toolchains::expand_in::<W>(t, |k| std::env::var(k).ok(), home.as_ref())
            }
        }
    }

    /// An XDG kind plus subpath, required to lie under `home`.
    ///
    /// The question is asked of the walk, by containment of positions, fs
    /// authority being over objects, so the freeze guard and the fs guard
    /// cannot disagree: `xdg-config ../../etc` lands on `/etc` and not under
    /// `home`, an `XDG_*_HOME` pointing *through* a symlink is judged where it
    /// lands, and a HOME that is itself a symlink (macOS `/home`) still
    /// contains its own subdirectories.
    ///
    /// That guard is stated in `home`'s terms, so an unknown home leaves an
    /// `xdg-*` grant unanswerable, an absolute `XDG_*_HOME` included, since
    /// there would then be nothing to contain it.
    fn xdg<W: Polar>(&self, kind: XdgKind, sub: &str) -> Result<Placed<W>, PolicyError> {
        let spelled = self.home();
        let (Some(spelled), Some(base)) = (
            spelled.as_deref(),
            resolve_xdg_in(|k| self.env.get_or_host(k), kind, spelled.as_deref()),
        ) else {
            return Err(home_unknown());
        };
        let tag = format!("xdg-{}", <&str>::from(kind));
        let resolved = GrantPath::<W>::walked(anchored(&tag, join_sub(base, sub)?)?)?;
        let lands = resolved.real.followed().real();
        let home = Home::of(spelled)?;
        if home.real.as_ref().is_some_and(|home| home.contains(lands)) {
            return Ok(Placed::Walked(resolved));
        }
        let var = kind.env_var();
        let val = self.env.get_or_host(var).unwrap_or_default();
        let name = <&str>::from(kind);
        let written = resolved.written.shown();
        let clause = if val.is_empty() {
            format!(
                "{var} is unset, so the default ({written}) was used. Is HOME ({}) set correctly?",
                shown(spelled),
            )
        } else {
            format!(
                "{var}={val}: set it to a subpath of HOME ({}), unset it to use the \
                 default, or replace {} in the policy with an {} path.",
                shown(spelled),
                code(&format!("`xdg-{name}")),
                code("`absolute"),
            )
        };
        let via = if lands.as_path().as_os_str() == resolved.written.as_str() {
            String::new()
        } else {
            format!(" (written '{written}', which is a symbolic link)")
        };
        Err(PolicyError::new(format!(
            "{} resolves to '{lands}'{via}, outside HOME; refusing to widen the grant.  {clause}",
            code(&format!("`xdg-{name}")),
        )))
    }
}

/// The home directory: as spelled, and where its walk lies, `None` for one
/// that is not absolute and so lies nowhere.
#[derive(Debug, Clone)]
pub(crate) struct Home {
    pub(crate) spelled: String,
    pub(crate) real: Option<RealPath>,
}

impl Home {
    /// `spelled`, walked.
    ///
    /// # Errors
    /// A home whose walk fails, named.
    pub(crate) fn of(spelled: &str) -> Result<Self, PolicyError> {
        let written = FrozenPath::from_surface(spelled);
        let real = written
            .is_absolute()
            .then(|| walk_lenient(&written.lexical(), LeafMode::Follow))
            .transpose()
            .map_err(|e| PolicyError::unwalkable(&written, &e))?
            .map(|trace| trace.real().clone());
        Ok(Self {
            spelled: spelled.into(),
            real,
        })
    }
}

/// An anchored place must freeze absolute: a relative `HOME` or `TMPDIR`
/// would otherwise re-anchor to the *live* cwd at check time, a different
/// directory after a `cd`.
fn anchored(tag: &str, frozen: FrozenPath) -> Result<FrozenPath, PolicyError> {
    if frozen.is_absolute() {
        return Ok(frozen);
    }
    Err(PolicyError::new(format!(
        "{} freezes to the relative path '{}'; an anchor must be absolute. \
         Is HOME or TMPDIR relative?",
        code(&format!("`{tag}")),
        frozen.shown()
    )))
}

/// Whether the raw `absolute` payload is a live path under `rules`, judged
/// before any folding: host folding would read `C:/../x` on POSIX as the
/// relative `x`.  A malformed root, or a head of two mixed separators
/// (`/\x`, `\/x`), is refused first, though Windows would read either as
/// absolute: the mixed head is one spelling POSIX and Windows read as
/// different paths, and refusing it on every host keeps a profile portable.
/// Then host first: live when [`grant_eligibility`] admits `rules`,
/// `Ok(false)` (the dead grant) when only the other platform's, refused when
/// under neither.
///
/// # Errors
/// A run-time prefix, a root-relative `\x`, a malformed UNC, verbatim or
/// device root, a head of two mixed separators, or a relative or
/// drive-relative path.
pub(crate) fn live_absolute(raw: &str, rules: PathRules) -> Result<bool, PolicyError> {
    if let Some(err) = runtime_prefix(raw) {
        return Err(err);
    }
    let live = grant_eligibility(raw)
        .map_err(|why| not_absolute(raw, why))?
        .admits(rules);
    if live {
        debug_assert!(
            rules != PathRules::Windows || !windows_identity_components(raw).is_empty(),
            "a live Windows root must name a component, or it prefixes every path"
        );
    }
    Ok(live)
}

/// A path the host holds, frozen as an `absolute` grant naming it would be.
///
/// Classified by [`live_absolute`] under the host's rules on its native
/// text, and frozen in its native form, never re-read through a payload
/// string.  A held path has no other platform to be live on, so where a
/// payload would be the dead grant this one is refused, as one absolute
/// under neither platform's rules is: a deny that named nothing would fail
/// open.
///
/// # Errors
/// A path that is not UTF-8, a malformed root, a path not absolute under
/// the host's rules, or one [`FrozenPath::freeze`] refuses.
pub fn host_absolute(path: &Path) -> Result<FrozenPath, PolicyError> {
    let text = crate::path::utf8(path)
        .map_err(|e| PolicyError::new(format!("{e}, so ral cannot hold it as a grant")))?;
    if live_absolute(text, PathRules::HOST)? {
        freeze(path)
    } else {
        Err(not_absolute(text, GrantRefusal::ForeignOrRelative))
    }
}

/// The refusal of `raw` by its [`GrantRefusal`]: a malformed or mixed head, or
/// one absolute under neither platform's rules.
fn not_absolute(raw: &str, why: GrantRefusal) -> PolicyError {
    let word = quote_word(raw);
    let abs = |rest: &str| code(&format!("`absolute {rest}"));
    PolicyError::new(match why {
        GrantRefusal::Malformed => format!(
            "{} is no well-formed UNC or verbatim path; a UNC path needs a \
             server and a share, as in {}; a verbatim path is \
             {} or {}.  A '?' is no server name, \
             '.' and '..' are neither a server nor a share name, and device paths \
             are not accepted",
            abs(&word),
            abs("'\\\\server\\share\\x'"),
            abs("'\\\\?\\C:\\x'"),
            abs("'\\\\?\\UNC\\server\\share\\x'"),
        ),
        GrantRefusal::MixedHead => format!(
            "{} opens with two different separators, which Windows reads as a \
             UNC or device path and POSIX does not; write {} or {}",
            abs(&word),
            abs("'\\\\server\\share\\x'"),
            abs("/x"),
        ),
        GrantRefusal::NtObject => format!(
            "{} is an NT object-manager path, not a path a grant can name; write {}",
            abs(&word),
            abs("'C:\\x'")
        ),
        GrantRefusal::Driveless => {
            format!("{} names no drive; write {}", abs(&word), abs("'C:\\x'"))
        }
        GrantRefusal::DriveRelative => {
            format!("{} is drive-relative; write {}", abs(&word), abs("'C:\\…'"))
        }
        GrantRefusal::ForeignOrRelative => format!(
            "{} needs an absolute path; for a path under the working \
             directory write {}",
            code("`absolute"),
            code(&format!("`cwd {word}")),
        ),
    })
}

/// A relative subpath, judged lexically so that a profile fails alike on
/// every platform: the empty string, a leading separator and a drive prefix
/// are refused, where `Path::join` would strip, replace or ignore them.
fn subpath<'a>(tag: &str, s: &'a str) -> Result<&'a str, PolicyError> {
    if let Some(err) = runtime_prefix(s) {
        return Err(err);
    }
    classify_subpath(s).map(|()| s).map_err(|why| {
        let word = quote_word(s);
        let this = code(&format!("`{tag}"));
        let drive = code("`absolute 'C:\\…'");
        PolicyError::new(match why {
            SubpathRefusal::Empty => format!(
                "{this} needs a path; write {} for the anchor itself",
                code(&format!("`{tag} ."))
            ),
            SubpathRefusal::Drive => {
                format!("{this} takes a relative path, not {word}; a drive belongs in {drive}")
            }
            SubpathRefusal::MixedHead => format!(
                "{this} takes a relative path, not {word}, which opens with two \
                 different separators"
            ),
            SubpathRefusal::Rooted => format!(
                "{this} takes a relative path, not {word}; for {word} write {}",
                code(&format!("`absolute {word}"))
            ),
            SubpathRefusal::NtObject => format!(
                "{this} takes a relative path, not {word}, an NT object-manager path, \
                 which no grant can name"
            ),
            SubpathRefusal::Driveless => format!(
                "{this} takes a relative path, not {word}, which names no drive; a drive \
                 belongs in {drive}"
            ),
        })
    })
}

/// `~` and `xdg:NAME` are expanded by the resolver at run time, not when a
/// grant is decoded, so under any tag they would name a directory literally
/// called `~`: refuse, and say which place the author meant.  The grant rule
/// is judged under Windows rules on every host, so `xdg:config\x` is an
/// `xdg:` head everywhere and the suffix is read to match.
fn runtime_prefix(s: &str) -> Option<PolicyError> {
    let rest = |r: &str| match relative_suffix(r, PathRules::Windows) {
        "" => ".".to_string(),
        r => quote_word(r).into_owned(),
    };
    let head = SigilHead::of_for(s, PathRules::Windows)?;
    Some(PolicyError::new(match head {
        SigilHead::Tilde(tilde) => format!(
            "'~' is not expanded in a grant; for a path under the home directory \
             write {}",
            code(&format!(
                "`home {}",
                rest(tilde.suffix.as_deref().unwrap_or_default())
            )),
        ),
        SigilHead::Xdg(kind, sub) => {
            let name = <&str>::from(kind);
            format!(
                "{} is not expanded in a grant; write {}",
                code(&format!("`xdg:{name}")),
                code(&format!("`xdg-{name} {}", rest(sub.unwrap_or_default()))),
            )
        }
        SigilHead::UnknownXdg => unknown_xdg_message(s),
    }))
}

/// The one answer for a home-relative place where nothing binds `HOME`: `home`
/// and `xdg-*` fail for the same reason and take the same two fixes.
fn home_unknown() -> PolicyError {
    PolicyError::new(format!(
        "HOME is unset, so {} and {} places in the policy \
         can't be resolved.  Set HOME in the environment, or replace the \
         home-relative places in the policy with {} paths.",
        code("`home"),
        code("`xdg-*"),
        code("`absolute"),
    ))
}

fn unknown_xdg_message(entry: &str) -> String {
    format!(
        "unknown xdg token '{entry}'; the places are {}. \
         Did you mean one of those?",
        XdgKind::VARIANTS
            .iter()
            .map(|&k| code(&format!("`xdg-{}", <&str>::from(k))))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn join_sub(base: impl AsRef<Path>, sub: &str) -> Result<FrozenPath, PolicyError> {
    freeze(base.as_ref().join(sub))
}

/// The grant door of [`FrozenPath`]: a grant is serialized as text, so a
/// spelling that is not Unicode is refused here.
fn freeze(path: impl AsRef<Path>) -> Result<FrozenPath, PolicyError> {
    FrozenPath::freeze(path.as_ref()).map_err(PolicyError::new)
}

/// A directory chosen for `selector` that is the root, the home or an
/// ancestor of it would make running reads a read of everything, `~/.ssh`
/// included.  Judged on where the directory's walk landed, by containment of
/// positions as [`FreezeCtx::xdg`] judges, so a home that is a symlink is
/// still caught.  `subject` says what chose it; `var` is what to check.
///
/// # Errors
/// `dir` is the filesystem root, the home or an ancestor of it.
pub(super) fn refuse_the_world(
    selector: Selector,
    subject: &str,
    var: &str,
    dir: &Trace,
    home: Option<&Home>,
) -> Result<(), PolicyError> {
    let dir = dir.real();
    let home = home.and_then(|h| h.real.as_ref());
    let what = match home {
        _ if dir.is_universal() => "the filesystem root",
        Some(home) if dir.contains(home) && home.contains(dir) => "the home directory itself",
        Some(home) if dir.contains(home) => "an ancestor of the home directory",
        _ => return Ok(()),
    };
    let selector = code(&format!("`dir `{}", selector.name()));
    Err(PolicyError::new(format!(
        "{subject}, {what}, so {selector} would run and read all of it. Is {var} set correctly?"
    )))
}

/// [`path_dirs_in`] over the host's `$PATH`.
fn path_dirs<W: Polar>(home: Option<&Home>) -> Result<Vec<GrantPath<W::Grant>>, PolicyError> {
    #[allow(clippy::disallowed_methods, reason = "PATH, not an XDG basedir")]
    let path = std::env::var_os("PATH").unwrap_or_default();
    path_dirs_in::<W>(&path, home)
}

/// The absolute entries of `path` as written, each classified on its raw
/// spelling as an `absolute` payload is ([`host_absolute`]), then frozen and
/// walked: [`path_dirs`](crate::path::path_dirs) folds, and a fold before the
/// classification would read `///x` as `/x`.  A relative entry is the environment's business, not the grant author's, so unlike a relative
/// grant path it is dropped in silence; one naming the world is not
/// ([`refuse_the_world`]), nor one [`host_absolute`] refuses, absolute to
/// the kernel but under a malformed or mixed head, or not UTF-8: the
/// selector fails whole rather than freeze a path it cannot read.
fn path_dirs_in<W: Polar>(
    path: &OsStr,
    home: Option<&Home>,
) -> Result<Vec<GrantPath<W::Grant>>, PolicyError> {
    let dirs: Vec<_> = crate::path::path_entries(path)
        .filter(|entry| crate::path::is_absolute(entry))
        .map(|entry| {
            let subject = format!("PATH holds {}", shown(&entry));
            let held = host_absolute(&entry).map_err(|e| {
                PolicyError::new(format!(
                    "{subject}, which {} cannot hold: {}",
                    code("`dir `path"),
                    e.message
                ))
            })?;
            let dir = GrantPath::<W>::walked(held)?;
            refuse_the_world(Selector::Path, &subject, "PATH", dir.real.followed(), home)?;
            W::grant(dir)
        })
        .collect::<Result<_, _>>()?;
    if dirs.is_empty() {
        return Err(PolicyError::new(format!(
            "{} expands to zero absolute directories; \
             PATH is empty, unset, or contains only relative entries",
            code("`path")
        )));
    }
    Ok(dirs)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build real directories and symlinks to freeze grants against"
)]
mod tests {
    use super::*;
    use crate::capability::Extent;
    use crate::path::Excluded;
    use crate::path::lex::fold_dots;
    use crate::path::sigil::expand_path_prefix;

    fn cwd_at(cwd: &str) -> CwdPath {
        CwdPath::new(cwd.into()).expect("a fixture cwd is absolute")
    }

    fn ctx(home: &str, cwd: &str) -> FreezeCtx {
        FreezeCtx::at(std::iter::once(("HOME", home)).collect(), cwd_at(cwd))
    }

    /// A context whose overlay binds no `HOME`: the shape every home-relative
    /// place must refuse, under a host that binds none either
    /// ([`HOME_VARS`](crate::host::HOME_VARS) cleared).
    fn homeless_ctx(cwd: &str) -> FreezeCtx {
        FreezeCtx::at(EnvVars::new(), cwd_at(cwd))
    }

    /// The live places among `places`, frozen; a dead grant is dropped.
    fn frozen(places: &[Place], ctx: &FreezeCtx) -> Result<Vec<String>, PolicyError> {
        places
            .iter()
            .filter_map(|p| ctx.place(p).transpose())
            .map(|p| p.map(|p| p.as_str().to_string()))
            .collect()
    }

    fn cwd(s: &str) -> Place {
        Place::Cwd(s.into())
    }

    fn abs(s: &str) -> Place {
        Place::Absolute(s.into())
    }

    fn xdg(kind: XdgKind, s: &str) -> Place {
        Place::Xdg(kind, s.into())
    }

    fn refusal(place: &Place, ctx: &FreezeCtx) -> String {
        ctx.place(place).unwrap_err().message
    }

    #[cfg(unix)]
    #[test]
    fn path_entries_naming_the_world_are_refused() {
        for (home, entry, what) in [
            ("/h", "/", "the filesystem root"),
            ("/h", "/h", "the home directory itself"),
            ("/home/a", "/home", "an ancestor of the home directory"),
        ] {
            let path = format!("/usr/bin:{entry}");
            let home = Home::of(home).unwrap();
            let err = path_dirs_in::<Trace>(OsStr::new(&path), Some(&home)).unwrap_err();
            assert_eq!(
                err.message,
                format!(
                    "PATH holds {entry}, {what}, so `` `dir `path `` would run and read all \
                     of it. Is PATH set correctly?"
                ),
            );
        }
    }

    /// An entry absolute to the kernel under a malformed head, or not
    /// UTF-8, fails the selector whole rather than freeze some other path.
    #[test]
    fn path_entries_a_grant_cannot_hold_are_refused() {
        let malformed = if cfg!(windows) {
            r"C:\bin;\\\x\bin"
        } else {
            "/usr/bin:///x/bin"
        };
        let err = path_dirs_in::<Trace>(OsStr::new(malformed), None).unwrap_err();
        assert!(
            err.message.contains("no well-formed UNC"),
            "{}",
            err.message
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(
                err.message.contains("PATH holds ///x/bin"),
                "the entry is named as written, unfolded: {}",
                err.message
            );
            let err =
                path_dirs_in::<Trace>(OsStr::from_bytes(b"/usr/bin:/\xff"), None).unwrap_err();
            assert!(err.message.contains("is not UTF-8"), "{}", err.message);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_homeless_path_is_refused_only_for_the_root() {
        assert!(path_dirs_in::<Trace>(OsStr::new("/usr/bin:/h"), None).is_ok());
        assert!(path_dirs_in::<Trace>(OsStr::new("/usr/bin:/"), None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_home_subdirectory_on_path_is_accepted() {
        let home = Home::of("/h").unwrap();
        let path = OsStr::new("/usr/bin:/h/bin:relative");
        let dirs = path_dirs_in::<Trace>(path, Some(&home)).unwrap();
        let dirs: Vec<_> = dirs.iter().map(|d| d.written.as_str()).collect();
        assert_eq!(dirs, ["/usr/bin", "/h/bin"]);
    }

    // Unix-only: `PathBuf::join` yields `\` separators on Windows.
    #[cfg(unix)]
    #[test]
    fn freeze_expands_cwd_place() {
        let paths = frozen(&[cwd("."), cwd("src")], &ctx("/h", "/work/proj")).unwrap();
        assert_eq!(
            paths,
            vec!["/work/proj".to_string(), "/work/proj/src".to_string()]
        );
    }

    #[test]
    fn freeze_expands_tempdir_place() {
        let places = [Place::Tempdir(".".into()), Place::Tempdir("scratch".into())];
        let paths = frozen(&places, &ctx("/h", "/cwd")).unwrap();
        // macOS `TMPDIR` ends in `/`, which folding strips, so compare
        // against the same kernel rather than a literal.
        let temp = std::env::temp_dir();
        let fold = |p: &Path| fold_dots(p).to_string_lossy().into_owned();
        assert_eq!(paths[0], fold(&temp));
        assert_eq!(paths[1], fold(&temp.join("scratch")));
    }

    // Unix-only: `/tmp` is dead on Windows.
    #[cfg(unix)]
    #[test]
    fn freeze_leaves_absolute_paths_alone() {
        let paths = frozen(&[abs("/tmp"), abs("/etc/hosts")], &ctx("/h", "/cwd")).unwrap();
        assert_eq!(paths, vec!["/tmp".to_string(), "/etc/hosts".to_string()]);
    }

    /// The guard folds the whole prefix before comparing, so a `..` climb out
    /// of HOME is rejected at freeze rather than collapsing at match time.
    // Unix-only: the folded escape `/etc` is a Unix root.
    #[cfg(unix)]
    #[test]
    fn freeze_rejects_xdg_subpath_escaping_home() {
        let err = refusal(&xdg(XdgKind::Config, "../../../../etc"), &ctx("/h", "/cwd"));
        assert!(err.contains("outside HOME"), "{err}");
    }

    /// The escape the surface form hides: `XDG_DATA_HOME` naming a link
    /// *inside* HOME whose target is outside it.  The fs guard matches the
    /// symlink-followed form, so the freeze guard must ask it there too.
    // Unix-only: `std::os::unix::fs::symlink`, and `/etc` is a Unix root.
    #[cfg(unix)]
    #[test]
    fn freeze_rejects_xdg_home_symlinked_out_of_home() {
        let home = tempfile::tempdir().unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        let err = crate::test_env::with_var("XDG_DATA_HOME", Some(&link.to_string_lossy()), || {
            refusal(&xdg(XdgKind::Data, "."), &ctx(&home_str, "/cwd"))
        });
        assert!(err.contains("outside HOME"), "{err}");
        assert!(
            err.contains("/etc"),
            "the message must name where it lands: {err}"
        );
    }

    /// The dual, and the reason both sides are canonicalised: macOS reaches
    /// a `/var/folders/...` tempdir through a symlink, so canonicalising only
    /// the XDG side would refuse a HOME nobody tampered with.
    // Unix-only: pairs with the test above.
    #[cfg(unix)]
    #[test]
    fn freeze_accepts_xdg_home_under_a_symlinked_home() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let home_str = home.path().to_string_lossy().into_owned();
        crate::test_env::with_var("XDG_DATA_HOME", Some(&data.to_string_lossy()), || {
            frozen(&[xdg(XdgKind::Data, ".")], &ctx(&home_str, "/cwd")).unwrap();
        });
    }

    /// The overlay is the environment a freeze reads: its `XDG_CONFIG_HOME`
    /// inside its `HOME` is where `xdg-config` lands, whatever the host binds.
    // Unix-only: `/tmp`-style fixture paths.
    #[cfg(unix)]
    #[test]
    fn freeze_reads_xdg_from_the_overlay() {
        let home = tempfile::tempdir().unwrap();
        let (home, cfg) = (home.path().to_str().unwrap(), home.path().join("cfg"));
        let cfg = cfg.to_str().unwrap();
        let env = [("HOME", home), ("XDG_CONFIG_HOME", cfg)]
            .into_iter()
            .collect::<EnvVars>();
        let ctx = FreezeCtx::at(env, cwd_at("/cwd"));
        let paths = frozen(&[xdg(XdgKind::Config, "probe")], &ctx).unwrap();
        assert_eq!(paths, [format!("{cfg}/probe")]);
    }

    /// A HOME of a drive root contains every XDG base on it, as the fs guard
    /// reads a prefix: the freeze asks its question, not a lexical twin.  The
    /// bare `\` is drive-relative, so no longer a root to contain anything.
    #[cfg(windows)]
    #[test]
    fn freeze_accepts_xdg_under_a_drive_root_home() {
        crate::test_env::with_var("XDG_DATA_HOME", None, || {
            frozen(&[xdg(XdgKind::Data, ".")], &ctx(r"C:\", r"C:\cwd")).unwrap();
        });
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_relative_home_is_refused() {
        let err = refusal(&Place::Home(".".into()), &ctx(r"\", r"C:\cwd"));
        assert!(err.contains("relative"), "{err}");
    }

    /// `Path::join` on a rooted suffix discards the base, so a subpath that
    /// leads with a separator must not name the root: it is refused, and the
    /// run-time half of expansion still answers alike.
    // Unix-only: Linux XDG defaults.
    #[cfg(unix)]
    #[test]
    fn xdg_subpath_cannot_discard_its_base() {
        crate::test_env::with_var("XDG_CONFIG_HOME", None, || {
            let ctx = ctx("/h", "/cwd");
            let err = refusal(&xdg(XdgKind::Config, "/etc"), &ctx);
            assert!(err.contains("absolute"), "{err}");
            assert_eq!(
                expand_path_prefix("xdg:config//etc", &ctx.env),
                "/h/.config/etc"
            );
        });
    }

    // Unix-only: Unix path shapes.
    #[cfg(unix)]
    #[test]
    fn freeze_folds_dot_dot_in_absolute() {
        let paths = frozen(&[abs("/a/b/../c")], &ctx("/h", "/cwd")).unwrap();
        assert_eq!(paths, vec!["/a/c".to_string()]);
    }

    /// `..` folds at freeze and may leave the anchor of a non-XDG place.
    // Unix-only: Unix path shapes.
    #[cfg(unix)]
    #[test]
    fn cwd_dot_dot_leaves_the_working_directory() {
        let paths = frozen(&[cwd("../x")], &ctx("/h", "/work/proj")).unwrap();
        assert_eq!(paths, vec!["/work/x".to_string()]);
    }

    /// Refused on every platform, judged lexically, so a profile fails alike
    /// everywhere.
    #[test]
    fn a_bad_subpath_is_refused_under_every_tag() {
        let ctx = ctx("/h", "/cwd");
        for sub in ["", "/x", r"\x", "C:x", r"C:\x", "c:/x"] {
            for place in [
                cwd(sub),
                Place::Home(sub.into()),
                Place::Tempdir(sub.into()),
                Place::Gitdir(sub.into()),
                xdg(XdgKind::Cache, sub),
            ] {
                assert!(ctx.place(&place).is_err(), "{place:?} should be refused");
            }
        }
    }

    #[test]
    fn a_bad_subpath_says_what_to_write() {
        let ctx = ctx("/h", "/cwd");
        assert!(refusal(&cwd(""), &ctx).contains("`cwd . "));
        assert!(refusal(&cwd("/x"), &ctx).contains("write `` `absolute /x ``"));
        assert!(refusal(&cwd("C:x"), &ctx).contains("`absolute 'C:"));
        assert!(refusal(&cwd(r"\x"), &ctx).contains("names no drive"));
    }

    /// §2.1's table, row by row, under both rules: `Ok(true)` live,
    /// `Ok(false)` dead, `Err` refused.
    #[test]
    fn absolute_payloads_classify_on_the_raw_string() {
        use PathRules::{Posix, Windows};
        let live = |raw, rules| live_absolute(raw, rules).ok();
        for raw in ["/x", "/"] {
            assert_eq!(live(raw, Posix), Some(true), "{raw}");
            assert_eq!(live(raw, Windows), Some(false), "{raw}");
        }
        for raw in [
            r"C:\x",
            "C:/x",
            r"\\server\share\x",
            r"\\?\C:\x",
            r"\\?\UNC\srv\sh\x",
            "//?/UNC/srv/sh/x",
            "C:/../x",
        ] {
            assert_eq!(live(raw, Posix), Some(false), "{raw}");
            assert_eq!(live(raw, Windows), Some(true), "{raw}");
        }
        // Absolute under both: live on both.
        for raw in ["//srv/sh/x", "//srv/sh"] {
            assert_eq!(live(raw, Posix), Some(true), "{raw}");
            assert_eq!(live(raw, Windows), Some(true), "{raw}");
        }
        for raw in [
            r"\x",
            "//",
            "///x",
            "//srv",
            "//srv/",
            r"\\",
            r"\\srv",
            r"\\srv\",
            r"\\\srv\sh",
            r"\\?\UNC\",
            r"\\?\UNC\srv",
            "//?/UNC/",
            r"\\.\C:\x",
            r"/\x",
            "C:x",
            "src",
            "./src",
            "",
            "~",
            "xdg:data",
        ] {
            assert_eq!(live(raw, Posix), None, "{raw}");
            assert_eq!(live(raw, Windows), None, "{raw}");
        }
    }

    /// A malformed UNC or verbatim root is live on no host, so it cannot fold
    /// to the universal prefix; the refusal says what a well-formed one is.
    #[test]
    fn a_malformed_unc_or_verbatim_root_is_refused_everywhere() {
        for raw in [
            r"\\",
            "//",
            r"\\srv",
            r"\\srv\..\x",
            r"\\..\sh\x",
            r"\\?\UNC\srv",
            "//?/UNC/",
            r"\\.\C:\x",
        ] {
            for rules in [PathRules::Posix, PathRules::Windows] {
                let err = live_absolute(raw, rules).unwrap_err();
                assert!(
                    err.message.contains("server and a share"),
                    "{raw}: {}",
                    err.message
                );
            }
        }
    }

    /// Every root kind, under both rules: `absolute` classifies as it did
    /// when POSIX absoluteness was "`/`, then a `Posix` or `Unc` root", now
    /// that it is the kernel's "opens with `/`".  The verbatim roots spelled
    /// with `/` stay dead on POSIX by the grant rule, the malformed ones stay
    /// refused by the well-formedness check, and a refusal is the one its
    /// root names.
    #[test]
    fn absolute_classification_survives_the_kernel_rule() {
        use PathRules::{Posix, Windows};
        const LIVE: Option<bool> = Some(true);
        const DEAD: Option<bool> = Some(false);
        const REFUSED: Option<bool> = None;
        for (raw, posix, windows) in [
            ("/x", LIVE, DEAD),
            ("/", LIVE, DEAD),
            ("//srv/sh/x", LIVE, LIVE),
            (r"\\srv\sh", DEAD, LIVE),
            (r"\\srv\sh\x", DEAD, LIVE),
            (r"\\?\C:\x", DEAD, LIVE),
            (r"\\?\C:\", DEAD, LIVE),
            ("//?/C:/x", DEAD, LIVE),
            (r"/\?\C:\x", DEAD, LIVE),
            (r"\/?\C:\x", DEAD, LIVE),
            (r"\\?\UNC\srv\sh", DEAD, LIVE),
            (r"\\?\unc\srv\sh\x", DEAD, LIVE),
            ("//?/UNC/srv/sh", DEAD, LIVE),
            (r"/\?/unc\srv/sh", DEAD, LIVE),
            (r"C:\x", DEAD, LIVE),
            ("c:/x", DEAD, LIVE),
            (r"\\", REFUSED, REFUSED),
            ("//", REFUSED, REFUSED),
            ("///x", REFUSED, REFUSED),
            (r"\\srv", REFUSED, REFUSED),
            (r"\\srv\", REFUSED, REFUSED),
            ("//srv", REFUSED, REFUSED),
            ("//srv/", REFUSED, REFUSED),
            (r"\\\srv\sh", REFUSED, REFUSED),
            ("///srv/sh", REFUSED, REFUSED),
            (r"\\?\", REFUSED, REFUSED),
            (r"\\?", REFUSED, REFUSED),
            (r"\\?\UNC\", REFUSED, REFUSED),
            (r"\\?\UNC\srv", REFUSED, REFUSED),
            (r"\\?\UNC\srv\", REFUSED, REFUSED),
            (r"\\?\UNC\?\sh", REFUSED, REFUSED),
            (r"\\?\UNC\.\sh", REFUSED, REFUSED),
            ("//?/UNC/", REFUSED, REFUSED),
            ("//?/UNC/srv", REFUSED, REFUSED),
            (r"\\?\C:", REFUSED, REFUSED),
            (r"\\?\x", REFUSED, REFUSED),
            ("//?/x", REFUSED, REFUSED),
            (r"/\?\x", REFUSED, REFUSED),
            (r"\\.\C:\x", REFUSED, REFUSED),
            (r"\\.\pipe\x", REFUSED, REFUSED),
            ("//./pipe/x", REFUSED, REFUSED),
            (r"\\?\?\x", REFUSED, REFUSED),
            (r"\x", REFUSED, REFUSED),
            (r"\", REFUSED, REFUSED),
            ("C:x", REFUSED, REFUSED),
            ("C:", REFUSED, REFUSED),
            ("x", REFUSED, REFUSED),
            ("./x", REFUSED, REFUSED),
            ("", REFUSED, REFUSED),
            (r"/\x", REFUSED, REFUSED),
            (r"/\srv\sh", REFUSED, REFUSED),
            (r"\/srv/sh", REFUSED, REFUSED),
            (r"\??\C:\x", REFUSED, REFUSED),
            ("/??/C:/x", LIVE, DEAD),
        ] {
            for (rules, want) in [(Posix, posix), (Windows, windows)] {
                let got = live_absolute(raw, rules);
                assert_eq!(got.as_ref().ok().copied(), want, "{raw} {rules:?}");
                if let Err(err) = got {
                    assert_eq!(
                        err.message,
                        not_absolute(raw, grant_eligibility(raw).unwrap_err()).message,
                        "{raw} {rules:?}"
                    );
                }
            }
        }
    }

    /// The anchors are the one declaration of the place tags.
    #[test]
    fn anchors_round_trip_through_their_tags() {
        for anchor in Anchor::all() {
            assert_eq!(Anchor::of_tag(&anchor.tag()), Some(anchor));
            assert_eq!(anchor.place(String::new()).anchor(), anchor);
        }
        assert_eq!(Anchor::of_tag("cwdd"), None);
    }

    /// The dead grant is `Ok(None)`: it loads and names no path.
    #[test]
    fn a_foreign_rooted_absolute_is_dead_on_this_host() {
        let foreign = if cfg!(windows) { "/x" } else { r"C:\x" };
        assert!(ctx("/h", "/cwd").place(&abs(foreign)).unwrap().is_none());
    }

    /// Why `place` is refused as polarity `W`.
    fn refused_as<W: Polar>(place: &Place) -> String {
        match ctx("/h", "/w")
            .walked::<W>(place)
            .and_then(|placed| placed.map(Placed::grant).transpose())
        {
            Err(e) => e.message,
            Ok(_) => panic!("{place:?} was not refused"),
        }
    }

    /// A foreign spelling that folds to `/` under host rules is classified
    /// first, so it is dead and no root.
    #[cfg(unix)]
    #[test]
    fn a_foreign_spelling_that_folds_to_the_root_is_dead_not_universal() {
        for raw in ["//?/C:/../..", "//?/C:/..", r"\\?\C:\..\.."] {
            let ctx = ctx("/h", "/w");
            assert!(matches!(ctx.walked::<Trace>(&abs(raw)), Ok(None)), "{raw}");
            assert!(
                matches!(ctx.walked::<Excluded>(&abs(raw)), Ok(None)),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_malformed_root_is_refused_before_any_fold() {
        for refusal in [
            refused_as::<Trace>(&abs("//")),
            refused_as::<Excluded>(&abs("//")),
        ] {
            assert!(refusal.contains("server and a share"), "{refusal}");
        }
    }

    #[test]
    fn the_root_spelling_is_everywhere_on_every_host() {
        let root = ctx("/h", "/w").walked::<Trace>(&abs("/")).unwrap().unwrap();
        assert_eq!(root.grant().unwrap().real, Extent::Everywhere);
        assert!(refused_as::<Excluded>(&abs("/")).contains("filesystem root"));
        #[cfg(windows)]
        assert!(matches!(
            ctx("/h", "/w").walked::<Trace>(&abs("/x")),
            Ok(None)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_spelling_that_folds_to_the_root_is_the_root() {
        for raw in ["/..", "/./"] {
            let root = ctx("/h", "/w").walked::<Trace>(&abs(raw)).unwrap().unwrap();
            assert_eq!(root.grant().unwrap().real, Extent::Everywhere, "{raw}");
            assert!(
                refused_as::<Excluded>(&abs(raw)).contains("filesystem root"),
                "{raw}"
            );
        }
    }

    /// A path the host holds has no other platform: where a payload would
    /// be the dead grant, it is refused, never dropped.
    #[test]
    fn a_held_path_absolute_only_elsewhere_is_refused() {
        let foreign = if cfg!(windows) { "/x" } else { r"C:\x" };
        assert!(host_absolute(Path::new(foreign)).is_err());
        assert!(host_absolute(Path::new(r"\x")).is_err());
    }

    /// `\??\` is refused under every tag, saying what it is rather than
    /// that it names no drive.
    #[test]
    fn an_object_manager_path_is_refused_as_such() {
        let ctx = ctx("/h", "/cwd");
        for place in [abs(r"\??\C:\x"), cwd(r"\??\C:\x")] {
            let err = refusal(&place, &ctx);
            assert!(err.contains("NT object-manager path"), "{place:?}: {err}");
            assert!(!err.contains("names no drive"), "{place:?}: {err}");
        }
    }

    #[test]
    fn run_time_prefixes_are_refused_under_every_tag_naming_the_place() {
        let ctx = ctx("/h", "/cwd");
        for place in [abs("~/x"), cwd("~/x"), Place::Home("~".into())] {
            let err = refusal(&place, &ctx);
            assert!(err.contains("`home x") || err.contains("`home ."), "{err}");
        }
        for place in [abs("xdg:config/x"), cwd("xdg:config/x")] {
            let err = refusal(&place, &ctx);
            assert!(err.contains("`xdg-config x"), "{err}");
        }
    }

    /// The grant rule reads a prefix under Windows rules on every host, so a
    /// backslash ends the `xdg:` name and follows a `~` as a `/` does.
    #[test]
    fn a_backslash_run_time_prefix_is_refused_on_every_host() {
        for (raw, want) in [(r"xdg:config\x", "`xdg-config x"), (r"~\x", "`home x")] {
            let err = runtime_prefix(raw).unwrap_or_else(|| panic!("{raw} is not a prefix"));
            assert!(err.message.contains(want), "{raw}: {}", err.message);
        }
    }

    #[test]
    fn an_unknown_xdg_prefix_is_refused_with_the_known_kinds() {
        let err = refusal(&cwd("xdg:cofnig/x"), &ctx("/h", "/cwd"));
        assert!(err.contains("xdg:cofnig"), "got {err}");
        assert!(err.contains("xdg-config"), "should list known kinds: {err}");
    }

    #[test]
    fn a_relative_home_is_refused() {
        let err = refusal(&Place::Home("x".into()), &ctx("rel", "/cwd"));
        assert!(err.contains("relative"), "{err}");
    }

    /// A home-relative place has no answer where nothing binds `HOME`.  The
    /// freeze refuses and says which env var is missing; the runtime twin
    /// passes the entry through literally (resolved against `cwd`), so neither
    /// fabricates `/.gitconfig`.
    #[test]
    fn freeze_rejects_home_relative_places_without_home() {
        crate::test_env::with_vars_cleared(&crate::host::HOME_VARS, || {
            for place in [
                Place::Home(".gitconfig".into()),
                xdg(XdgKind::Config, "git"),
            ] {
                let err = refusal(&place, &homeless_ctx("/cwd"));
                assert!(err.contains("HOME is unset"), "{place:?}: {err}");
            }
            let env = EnvVars::new();
            assert_eq!(expand_path_prefix("~/.gitconfig", &env), "~/.gitconfig");
            assert_eq!(expand_path_prefix("xdg:config/git", &env), "xdg:config/git");
        });
    }
}
