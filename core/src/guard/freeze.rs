//! The freeze: the one place a grant's [`Place`]s and [`Selector`]s become
//! [`FrozenPath`]s, against one anchor, [`FreezeCtx`].
//!
//! A grant names no host's home, XDG layout or working directory: a place is
//! a tag and a subpath, `absolute /usr`, `cwd src`, `home .ssh`,
//! `tempdir .`, `gitdir .` or `xdg-NAME sub`, and the anchor is read here,
//! once, so a later `chdir` or `TMPDIR` change cannot retroactively widen a
//! grant.  A selector names directories the environment chooses, and is
//! exec-only: its expansion is held under no `home`, but a toolchain
//! override may not name the world ([`toolchains`]).  XDG
//! uses the Linux defaults on every platform, macOS included
//! ([`crate::host`]).
//!
//! An `absolute` payload is classified on the raw string, before any folding,
//! host first, so that one profile loads on both platforms: live if absolute
//! under this host's rules, dead (a grant naming no path here) if absolute
//! only under the other platform's, refused if under neither.  `/x` is
//! POSIX-absolute and names no drive, `C:\x` and `\\srv\sh\x` are
//! Windows-absolute, and a well-formed `//srv/sh/x` is both, so live on both.
//! A UNC root, plain or verbatim (`\\?\UNC\srv\sh`), needs a server and a
//! share; a bare `\\` or `//` is none, and no `\\?\` or `\\.\` path is
//! read as anything but a drive path or such a root.  A
//! subpath is judged lexically, so that a profile fails alike everywhere.
//! `~` and `xdg:NAME` are expanded by the resolver at run time, never in a
//! grant, and are refused under every tag.
//!
//! Two anchors read a source the session does not own, and each guards it in
//! the terms of what would otherwise author the grant.  An `XDG_*_HOME` must
//! land under `home`: a variable set to `/etc` would otherwise widen an
//! `xdg-data` grant to it.  And a `.git` *file* names its git directory in the
//! working tree's own words, so a `gitdir` that followed the pointer alone
//! would be authored by whoever wrote the tree, and the place is granted for
//! read *and* write.  Following it therefore asks the other direction too: the
//! git directory must name this working tree back, as a linked worktree's
//! `gitdir` file does and as `core.worktree` does for a repository split off
//! with `--separate-git-dir`.  A pointer no git directory claims is a
//! [`PolicyError`], never a wider grant.  `tempdir` alone trusts its source as
//! given, `TMPDIR` being the launching user's to set.

use super::toolchains::{self, Toolchain};
use crate::host::{XdgKind, resolve_xdg};
use crate::path::git::{BadPointer, Pointer, discover_git_dir};
use crate::path::identity::windows_identity_components;
use crate::path::lex::{Root, root};
use crate::path::sigil::parse_xdg_token;
use crate::path::tilde::TildePath;
use crate::path::{Allow, FrozenPath, PathRules};
use crate::syntax::quote::quote_word;
use crate::text::code;
use crate::types::{Break, Error, Shell};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
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

fn unclaimed_message(dot_git: &Path, target: &Path) -> PolicyError {
    PolicyError::new(format!(
        "the .git file at '{dot_git}' points at '{target}', but that directory \
         does not name this working tree back, so {gitdir} will not grant it; \
         a pointer written inside the tree would otherwise choose the grant.",
        dot_git = dot_git.display(),
        gitdir = code("`gitdir"),
        target = target.display(),
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
        dot_git.display(),
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
        dot_git.display(),
        code("`gitdir"),
    ))
    .with_hint(
        "The freeze reads the pointer as the user launching the session.  Check \
         the file's permissions, or replace `gitdir in the policy with an \
         explicit path.",
    )
}

/// The anchor a grant is frozen against: `xdg-*` and `tempdir` read the
/// process environment, and `gitdir` walks the filesystem from `cwd`.
///
/// `home` is `None` where nothing binds one, so a policy naming a
/// home-relative place there is refused rather than resolved against a
/// stand-in.  The `cwd` is not optional, unlike
/// [`Resolver`](crate::path::Resolver)'s otherwise identical pair: a grant is
/// frozen once, so "here" must be settled now rather than re-asked per access.
#[derive(Debug, Clone)]
pub struct FreezeCtx {
    pub home: Option<String>,
    pub cwd: PathBuf,
}

impl FreezeCtx {
    /// The shell's own anchor: the home its override chain binds and its
    /// logical cwd, so every freeze of one session agrees.
    pub fn of(shell: &Shell) -> Self {
        Self {
            home: shell.context.home(),
            cwd: shell.cwd(),
        }
    }

    /// The home to freeze against, an empty one read as none: the field is
    /// public, and this is the door where a `Some("")` would root `home x`
    /// at `/x`, a grant nobody wrote.
    fn home(&self) -> Option<&str> {
        self.home.as_deref().filter(|h| !h.is_empty())
    }

    /// Freeze one place: validate its payload, resolve its anchor, then fold
    /// `.`/`..` and wrap, so every frozen place is in the normal form the
    /// in-process guard matches against.  Resolving here at load rather than
    /// per check is what makes a grant immune to later env and cwd changes.
    ///
    /// `Ok(None)` is the dead grant: an `absolute` path rooted under the other
    /// platform's rules, which can never match an access here.
    ///
    /// # Errors
    /// A malformed payload, an `XDG_*_HOME` that escapes `HOME`, an unset
    /// `HOME` under a home-relative place, an anchored place that freezes
    /// relative, or a `.git` pointer no git directory claims.
    pub fn place(&self, place: &Place) -> Result<Option<FrozenPath>, PolicyError> {
        let tag = place.tag();
        let sub = |s| subpath(&tag, s);
        let frozen = match place {
            Place::Absolute(raw) => {
                return live_absolute(raw, PathRules::HOST)?
                    .then(|| freeze(raw))
                    .transpose();
            }
            Place::Cwd(s) => join_sub(&self.cwd, sub(s)?)?,
            Place::Tempdir(s) => join_sub(std::env::temp_dir(), sub(s)?)?,
            Place::Gitdir(s) => {
                let sub = sub(s)?;
                let base = discover_git_dir(&self.cwd)?.unwrap_or_else(|| self.cwd.clone());
                join_sub(base, sub)?
            }
            Place::Home(s) => {
                let sub = sub(s)?;
                join_sub(self.home().ok_or_else(home_unknown)?, sub)?
            }
            Place::Xdg(kind, s) => self.xdg(*kind, sub(s)?)?,
        };
        anchored(&tag, frozen).map(Some)
    }

    /// The directories a selector names.  Selectors read the host, not the
    /// anchor, save for the home; they sit here so that one resolver freezes
    /// every grant path.
    ///
    /// # Errors
    /// `path` over a `$PATH` with no absolute entry, or a toolchain override
    /// that names the filesystem root or the home.
    pub fn selector(&self, selector: Selector) -> Result<Vec<FrozenPath>, PolicyError> {
        match selector {
            Selector::Path => path_dirs(),
            Selector::Toolchain(t) => toolchains::expand(t, self.home())?
                .iter()
                .map(freeze)
                .collect(),
        }
    }

    /// An XDG kind plus subpath, required to lie under `home`.
    ///
    /// The question is asked exactly as the in-process fs guard asks it, by
    /// containment of the symlink-followed forms, fs authority being over
    /// objects, so the freeze guard and the fs guard cannot disagree.  Both
    /// sides are folded and canonicalised: `xdg-config ../../etc` collapses to
    /// `/etc` rather than stepping over the guard and collapsing at match
    /// time, an `XDG_*_HOME` pointing *through* a symlink is judged where it
    /// lands, and a HOME that is itself a symlink (macOS `/home`) still
    /// contains its own subdirectories.
    ///
    /// That guard is stated in `home`'s terms, so an unknown home leaves an
    /// `xdg-*` grant unanswerable, an absolute `XDG_*_HOME` included, since
    /// there would then be nothing to contain it.
    fn xdg(&self, kind: XdgKind, sub: &str) -> Result<FrozenPath, PolicyError> {
        let (Some(home), Some(base)) = (self.home(), resolve_xdg(kind, self.home())) else {
            return Err(home_unknown());
        };
        let resolved = join_sub(base, sub)?;
        if FrozenPath::from_surface(home).contains::<Allow>(resolved.real_path()) {
            return Ok(resolved);
        }
        let var = kind.env_var();
        let val = std::env::var(var).unwrap_or_default();
        let name = <&str>::from(kind);
        let clause = if val.is_empty() {
            format!(
                "{var} is unset, so the default ({}) was used. Is HOME ({home}) set correctly?",
                resolved.as_str(),
            )
        } else {
            format!(
                "{var}={val}: set it to a subpath of HOME ({home}), unset it to use the \
                 default, or replace {} in the policy with an {} path.",
                code(&format!("`xdg-{name}")),
                code("`absolute"),
            )
        };
        let via = if resolved.real() == resolved.as_str() {
            String::new()
        } else {
            format!(
                " (written '{}', which is a symbolic link)",
                resolved.as_str()
            )
        };
        Err(PolicyError::new(format!(
            "{} resolves to '{}'{via}, outside HOME; refusing to widen the grant.  {clause}",
            code(&format!("`xdg-{name}")),
            resolved.real(),
        )))
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
        frozen.as_str()
    )))
}

/// Whether the raw `absolute` payload is a live path under `rules`, judged
/// before any folding: host folding would read `C:/../x` on POSIX as the
/// relative `x`.  Host first: live when absolute under `rules`, `Ok(false)`
/// (the dead grant) when absolute only under the other platform's, refused
/// when under neither.
///
/// # Errors
/// A run-time prefix, a root-relative `\x`, a malformed UNC, verbatim or
/// device root, or a relative or drive-relative path.
pub(crate) fn live_absolute(raw: &str, rules: PathRules) -> Result<bool, PolicyError> {
    if let Some(err) = runtime_prefix(raw) {
        return Err(err);
    }
    let root = root(raw);
    let absolute = |rules| match rules {
        PathRules::Posix => raw.starts_with('/') && matches!(root, Root::Posix | Root::Unc),
        PathRules::Windows => matches!(root, Root::Unc | Root::Verbatim | Root::Drive),
    };
    let other = match rules {
        PathRules::Posix => PathRules::Windows,
        PathRules::Windows => PathRules::Posix,
    };
    if root == Root::Malformed {
        Err(not_absolute(raw, root))
    } else if absolute(rules) {
        debug_assert!(
            rules != PathRules::Windows || !windows_identity_components(raw).is_empty(),
            "a live Windows root must name a component, or it prefixes every path"
        );
        Ok(true)
    } else if absolute(other) {
        Ok(false)
    } else {
        Err(not_absolute(raw, root))
    }
}

fn not_absolute(raw: &str, root: Root) -> PolicyError {
    let word = quote_word(raw);
    let abs = |rest: &str| code(&format!("`absolute {rest}"));
    PolicyError::new(match root {
        Root::Malformed => format!(
            "{} is no well-formed UNC or verbatim path; a UNC path needs a \
             server and a share, as in {}; a verbatim path is \
             {} or {}.  A '?' or \
             '.' is no server name, and device paths are not accepted",
            abs(&word),
            abs("'\\\\server\\share\\x'"),
            abs("'\\\\?\\C:\\x'"),
            abs("'\\\\?\\UNC\\server\\share\\x'"),
        ),
        _ if raw.starts_with('\\') => {
            format!("{} names no drive; write {}", abs(&word), abs("'C:\\x'"))
        }
        _ if has_drive(raw) => {
            format!("{} is drive-relative; write {}", abs(&word), abs("'C:\\…'"))
        }
        _ => format!(
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
    let word = quote_word(s);
    let this = code(&format!("`{tag}"));
    let drive = code("`absolute 'C:\\…'");
    match s.as_bytes() {
        [] => Err(PolicyError::new(format!(
            "{this} needs a path; write {} for the anchor itself",
            code(&format!("`{tag} ."))
        ))),
        [b'/', ..] => Err(PolicyError::new(format!(
            "{this} takes a relative path, not {word}; for {word} write {}",
            code(&format!("`absolute {word}"))
        ))),
        [b'\\', ..] => Err(PolicyError::new(format!(
            "{this} takes a relative path, not {word}, which names no drive; a drive \
             belongs in {drive}"
        ))),
        _ if has_drive(s) => Err(PolicyError::new(format!(
            "{this} takes a relative path, not {word}; a drive belongs in {drive}"
        ))),
        _ => Ok(s),
    }
}

pub(crate) fn has_drive(s: &str) -> bool {
    matches!(s.as_bytes(), [d, b':', ..] if d.is_ascii_alphabetic())
}

/// `~` and `xdg:NAME` are expanded by the resolver at run time, not when a
/// grant is decoded, so under any tag they would name a directory literally
/// called `~`: refuse, and say which place the author meant.
fn runtime_prefix(s: &str) -> Option<PolicyError> {
    let rest = |r: &str| match r.trim_start_matches(['/', '\\']) {
        "" => ".".to_string(),
        r => quote_word(r).into_owned(),
    };
    if let Some(tilde) = TildePath::parse(s) {
        return Some(PolicyError::new(format!(
            "'~' is not expanded in a grant; for a path under the home directory \
             write {}",
            code(&format!(
                "`home {}",
                rest(tilde.suffix.as_deref().unwrap_or_default())
            )),
        )));
    }
    if let Some((kind, sub)) = parse_xdg_token(s) {
        let name = <&str>::from(kind);
        return Some(PolicyError::new(format!(
            "{} is not expanded in a grant; write {}",
            code(&format!("`xdg:{name}")),
            code(&format!("`xdg-{name} {}", rest(sub.unwrap_or_default()))),
        )));
    }
    s.starts_with("xdg:")
        .then(|| PolicyError::new(unknown_xdg_message(s)))
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

/// The grant door of [`FrozenPath`]: an allowance keeps the real path it is
/// frozen to for good, so one that is not Unicode is refused here.
fn freeze(path: impl AsRef<Path>) -> Result<FrozenPath, PolicyError> {
    FrozenPath::freeze(path).map_err(PolicyError::new)
}

/// `$PATH` split on the platform separator, keeping the absolute entries.  A
/// relative `$PATH` entry is the environment's business, not the grant
/// author's, so unlike a relative grant path it is dropped in silence.
fn path_dirs() -> Result<Vec<FrozenPath>, PolicyError> {
    #[allow(clippy::disallowed_methods, reason = "PATH, not an XDG basedir")]
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<_> = std::env::split_paths(&path)
        .filter(|entry| entry.is_absolute())
        .map(freeze)
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
    use crate::path::lex::fold_dots;
    use crate::path::sigil::expand_path_prefix;

    fn ctx(home: &str, cwd: &str) -> FreezeCtx {
        FreezeCtx {
            home: Some(home.into()),
            cwd: cwd.into(),
        }
    }

    /// A context where nothing binds `HOME`: the shape every home-relative
    /// place must refuse.
    fn homeless_ctx(cwd: &str) -> FreezeCtx {
        FreezeCtx {
            home: None,
            cwd: cwd.into(),
        }
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
            let err = refusal(&xdg(XdgKind::Config, "/etc"), &ctx("/h", "/cwd"));
            assert!(err.contains("absolute"), "{err}");
            assert_eq!(
                expand_path_prefix("xdg:config//etc", Some("/h")),
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
        for raw in ["/x", "/", "/\\x"] {
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
    /// passes the entry through literally, a prefix matching nothing being the
    /// fail-closed reading on that side.  Neither fabricates `/.gitconfig`.
    #[test]
    fn freeze_rejects_home_relative_places_without_home() {
        for place in [
            Place::Home(".gitconfig".into()),
            xdg(XdgKind::Config, "git"),
        ] {
            let err = refusal(&place, &homeless_ctx("/cwd"));
            assert!(err.contains("HOME is unset"), "{place:?}: {err}");
        }
        assert_eq!(expand_path_prefix("~/.gitconfig", None), "~/.gitconfig");
        assert_eq!(expand_path_prefix("xdg:config/git", None), "xdg:config/git");
    }
}
