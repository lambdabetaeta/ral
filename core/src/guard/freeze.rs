//! The freeze: the one place a grant string becomes a [`FrozenPath`], against
//! one anchor, [`FreezeCtx`].
//!
//! A grant names no host's home, XDG layout or working directory: its paths
//! lead with a sigil, `~[/sub]`, `xdg:NAME[/sub]`, `cwd:[/sub]`,
//! `tempdir:[/sub]` or `gitdir:[/sub]`, and anything else passes through
//! unchanged.  `~` and `xdg:` expand at run time too (stage 1 of
//! [`Resolver`](crate::path::Resolver), by [`crate::path::sigil`]); the other
//! three are freeze-only and resolve exactly once, so a later `chdir` or
//! `TMPDIR` change cannot retroactively widen a grant.  `path:` and
//! `system:` are exec-only, each expanding to many directories rather than
//! one path, and [`super::decode`]'s exec-map freeze handles them with
//! [`path_dirs`] and [`system_dirs`].  XDG uses the Linux defaults on every
//! platform, macOS included ([`crate::host`]).
//!
//! Two sigils read a source the session does not own, and each guards it in
//! the terms of what would otherwise author the grant.  An `XDG_*_HOME` must
//! land under `home`: a variable set to `/etc` would otherwise widen an
//! `xdg:data` grant to it.  And a `.git` *file* names its git directory in the
//! working tree's own words, so a `gitdir:` that followed the pointer alone
//! would be authored by whoever wrote the tree, and the sigil is granted for
//! read *and* write.  Following it therefore asks the other direction too: the
//! git directory must name this working tree back, as a linked worktree's
//! `gitdir` file does and as `core.worktree` does for a repository split off
//! with `--separate-git-dir`.  A pointer no git directory claims is a
//! [`PolicyError`], never a wider grant.  `tempdir:` alone trusts its source as
//! given, `TMPDIR` being the launching user's to set.

use crate::host::{XdgKind, resolve_xdg};
use crate::path::git::{BadPointer, Pointer, discover_git_dir};
use crate::path::lex::is_foreign_rooted;
use crate::path::sigil::{parse_xdg_token, relative_suffix};
use crate::path::tilde::TildePath;
use crate::path::{Allow, FrozenPath, PathRules};
use crate::types::{Break, Error, Shell};
use std::path::{Path, PathBuf};
use strum::VariantArray as _;

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

/// The `gitdir:` sigil's refusals, in the grant's terms.
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
         does not name this working tree back, so `gitdir:` will not grant it; \
         a pointer written inside the tree would otherwise choose the grant.",
        dot_git = dot_git.display(),
        target = target.display(),
    ))
    .with_hint(
        "A linked worktree's git directory holds a `gitdir` file naming this \
         very .git file, and a repository split off with `--separate-git-dir` \
         holds `core.worktree` in its config; neither names this tree.  Was the \
         worktree moved?  `git worktree repair` rewrites both ends.  If the \
         pointer is hand-written, name the git directory explicitly in the \
         policy instead of `gitdir:`.",
    )
}

fn no_pointer_message(dot_git: &Path) -> PolicyError {
    PolicyError::new(format!(
        "the .git entry at '{}' is a file with no `gitdir:` line, so there is \
         no git directory for `gitdir:` to name.",
        dot_git.display(),
    ))
    .with_hint(
        "A worktree's or submodule's `.git` file holds one line, \
         `gitdir: <path>`.  Is this file something else?  If the tree is not a \
         repository, drop `gitdir:` from the policy or replace it with an \
         explicit path.",
    )
}

fn unreadable_message(dot_git: &Path, error: &std::io::Error) -> PolicyError {
    PolicyError::new(format!(
        "the .git file at '{}' cannot be read ({error}), so `gitdir:` cannot \
         say which git directory the grant covers.",
        dot_git.display(),
    ))
    .with_hint(
        "The freeze reads the pointer as the user launching the session.  Check \
         the file's permissions, or replace `gitdir:` in the policy with an \
         explicit path.",
    )
}

/// The anchor a grant is frozen against: `xdg:` and `tempdir:` read the
/// process environment, and `gitdir:` walks the filesystem from `cwd`.
///
/// `home` is `None` where nothing binds one, so a policy naming a
/// home-relative sigil there is refused rather than resolved against a
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
    /// public, and this is the door where a `Some("")` would root `~/x` at
    /// `/x`, a grant nobody wrote.
    fn home(&self) -> Option<&str> {
        self.home.as_deref().filter(|h| !h.is_empty())
    }

    /// Expand one entry's sigil, then fold `.`/`..` and wrap, so every frozen
    /// entry is sigil-free and in the normal form the in-process guard matches
    /// against.
    ///
    /// # Errors
    /// An unknown `xdg:` token, an `xdg:` path that escapes `HOME` once folded,
    /// an unset `HOME` under a home-relative sigil (`~`, `xdg:`), or a `.git`
    /// pointer no git directory claims.
    pub fn path(&self, entry: &str) -> Result<FrozenPath, PolicyError> {
        if entry.starts_with("xdg:") {
            let (kind, sub) = parse_xdg_token(entry)
                .ok_or_else(|| PolicyError::new(unknown_xdg_message(entry)))?;
            return self.xdg(kind, sub);
        }
        if let Some(sub) = parse_literal_sigil(entry, "cwd") {
            return Ok(join_sub(self.cwd.clone(), sub));
        }
        if let Some(sub) = parse_literal_sigil(entry, "tempdir") {
            return Ok(join_sub(std::env::temp_dir(), sub));
        }
        if let Some(sub) = parse_literal_sigil(entry, "gitdir") {
            let base = discover_git_dir(&self.cwd)?.unwrap_or_else(|| self.cwd.clone());
            return Ok(join_sub(base, sub));
        }
        if let Some(tilde) = TildePath::parse(entry) {
            let home = self.home().ok_or_else(home_unknown)?;
            return Ok(FrozenPath::from_surface(tilde.expand(home)));
        }
        Ok(FrozenPath::from_surface(entry))
    }

    /// [`path`](Self::path) over a list, minting the grant's whole prefix set
    /// at once.  Resolving here at load rather than per check is what makes a
    /// grant immune to later env and cwd changes, and closes the window in
    /// which `XDG_*_HOME` could mutate between load and a subsequent access.
    ///
    /// # Errors
    /// Whatever [`path`](Self::path) rejects, on the first entry that does.
    pub fn paths(&self, entries: &[String]) -> Result<Vec<FrozenPath>, PolicyError> {
        entries.iter().map(|entry| self.path(entry)).collect()
    }

    /// [`path`](Self::path), requiring the result absolute.  Shared by the fs
    /// prefix lists and the exec map keys.
    ///
    /// A path rooted under a foreign platform's convention (`/usr/local/bin`
    /// on a Windows build) is `Ok(None)`: a dead grant, which can never match
    /// an access here.  A genuinely relative entry is an error instead: it
    /// would re-anchor to the *live* cwd at check time, a different directory
    /// after a `cd`, so the message points at `cwd:`, which pins "relative to
    /// here" at freeze.
    ///
    /// # Errors
    /// Whatever [`path`](Self::path) rejects, or a relative entry.
    pub fn absolute(
        &self,
        entry: &str,
        err_prefix: &str,
    ) -> Result<Option<FrozenPath>, PolicyError> {
        let frozen = self.path(entry)?;
        if frozen.is_absolute() {
            return Ok(Some(frozen));
        }
        if is_foreign_rooted(frozen.as_str(), PathRules::HOST) {
            return Ok(None);
        }
        Err(PolicyError::new(format!(
            "{err_prefix}: relative path '{entry}' is not allowed; \
             use an absolute path, or cwd:{entry} for \"relative to here\""
        )))
    }

    /// An XDG kind plus sub-path, required to lie under `home`.
    ///
    /// The question is asked exactly as the in-process fs guard asks it, by
    /// containment of the symlink-followed forms, fs authority being over
    /// objects, so the freeze guard and the fs guard cannot disagree.  Both
    /// sides are folded and canonicalised: `xdg:config/../../etc` collapses to
    /// `/etc` rather than stepping over the guard and collapsing at match
    /// time, an `XDG_*_HOME` pointing *through* a symlink is judged where it
    /// lands, and a HOME that is itself a symlink (macOS `/home`) still
    /// contains its own subdirectories.
    ///
    /// That guard is stated in `home`'s terms, so an unknown home leaves an
    /// `xdg:` grant unanswerable, an absolute `XDG_*_HOME` included, since
    /// there would then be nothing to contain it.
    fn xdg(&self, kind: XdgKind, sub: Option<&str>) -> Result<FrozenPath, PolicyError> {
        let (Some(home), Some(base)) = (self.home(), resolve_xdg(kind, self.home())) else {
            return Err(home_unknown());
        };
        let resolved = join_sub(base, sub);
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
                 default, or replace xdg:{name} in the policy with an explicit path."
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
            "xdg:{name} resolves to '{}'{via}, outside HOME; refusing to widen the grant.  {clause}",
            resolved.real(),
        )))
    }
}

/// The one answer for a home-relative sigil where nothing binds `HOME`: `~`
/// and `xdg:` fail for the same reason and take the same two fixes.
fn home_unknown() -> PolicyError {
    PolicyError::new(
        "HOME is unset, so `~/...` and `xdg:...` tokens in the policy \
         can't be resolved.  Set HOME in the environment, or replace the \
         sigil-bearing entries in the policy with explicit absolute paths.",
    )
}

fn unknown_xdg_message(entry: &str) -> String {
    format!(
        "unknown xdg token '{entry}'; known kinds are: {}. \
         Did you mean one of those? (Token form is `xdg:NAME` or \
         `xdg:NAME/sub/path`.)",
        XdgKind::VARIANTS
            .iter()
            .map(|&k| <&str>::from(k))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Match `name:`, `name:sub`, or `name:/sub`, the suffix made
/// [`relative_suffix`]-safe.
#[allow(
    clippy::option_option,
    reason = "tri-state: no-match / match-no-suffix / match-with-suffix"
)]
fn parse_literal_sigil<'a>(input: &'a str, name: &str) -> Option<Option<&'a str>> {
    let body = input.strip_prefix(name)?.strip_prefix(':')?;
    Some((!body.is_empty()).then(|| relative_suffix(body)))
}

fn join_sub(base: PathBuf, sub: Option<&str>) -> FrozenPath {
    let full = match sub {
        None | Some("") => base,
        Some(s) => base.join(s),
    };
    FrozenPath::from_surface(&full)
}

/// A path separator, or one of the five sigil heads [`FreezeCtx::path`] knows:
/// what lets a bare command name (`git`) through the exec map's freeze
/// unresolved.
pub(crate) fn looks_like_path_or_sigil(s: &str) -> bool {
    s.contains('/')
        || TildePath::parse(s).is_some()
        || ["xdg:", "cwd:", "tempdir:", "gitdir:"]
            .iter()
            .any(|sigil| s.starts_with(sigil))
}

/// `$PATH` split on the platform separator, keeping the absolute entries.  A
/// relative `$PATH` entry is the environment's business, not the grant
/// author's, so unlike a relative grant path it is dropped in silence.
pub(crate) fn path_dirs(err_prefix: &str) -> Result<Vec<FrozenPath>, PolicyError> {
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs: Vec<_> = std::env::split_paths(&path)
        .filter(|entry| entry.is_absolute())
        .map(FrozenPath::from_surface)
        .collect();
    if dirs.is_empty() {
        return Err(PolicyError::new(format!(
            "{err_prefix}: 'path:' expands to zero absolute directories; \
             PATH is empty, unset, or contains only relative entries"
        )));
    }
    Ok(dirs)
}

/// `system:` expansion.  Unlike `path:` it can never come back empty, the
/// platform's own tool roots being unconditional, so there is no
/// empty-expansion error to raise here.
pub(crate) fn system_dirs() -> Vec<FrozenPath> {
    system_tool_roots()
        .iter()
        .map(FrozenPath::from_surface)
        .collect()
}

/// The platform's tool-root directories, what the `system:` exec sigil expands
/// to.
///
/// Feeds [`unix_tool_roots`] or [`windows_tool_roots`] the live filesystem and
/// environment.  Both stay parameterised over those inputs so each platform's
/// list is unit-testable on every host, not only the one compiling it, the
/// pattern `which`'s `name_key_on` follows too.
pub fn system_tool_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        let system_root = std::env::var("SystemRoot").unwrap_or_default();
        let program_files = std::env::var("ProgramFiles").unwrap_or_default();
        let program_files_x86 = std::env::var("ProgramFiles(x86)").unwrap_or_default();
        let program_files_dirs: Vec<&str> = [program_files.as_str(), program_files_x86.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        windows_tool_roots(&system_root, &program_files_dirs, crate::path::exists)
    }
    #[cfg(not(windows))]
    {
        unix_tool_roots(crate::path::exists)
    }
}

/// `/usr/bin` and `/bin` unconditionally, plus whichever of the Homebrew
/// prefixes and the toolchain roots `exists` reports.
///
/// The one place platform tool roots live as grant data: the OS sandboxes
/// admit nothing beyond what a grant names, so program behaviour that no
/// carrier explains (`cc → cc1`, `git → git-remote-https`, `clang → ld`)
/// needs its directory here.
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
pub(crate) fn unix_tool_roots(exists: impl Fn(&str) -> bool) -> Vec<String> {
    let mut roots = vec!["/usr/bin".to_string(), "/bin".to_string()];
    roots.extend(
        [
            "/opt/homebrew",
            "/home/linuxbrew/.linuxbrew",
            "/usr/libexec",
            "/usr/lib/git-core",
            "/usr/lib/gcc",
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
        ]
        .into_iter()
        .filter(|root| exists(root))
        .map(str::to_string),
    );
    roots
}

/// `%SystemRoot%\System32` and the bundled Windows PowerShell home, falling
/// back to the conventional `C:\Windows` when `system_root` is empty.
///
/// A Git-for-Windows `usr\bin` joins them, under whichever
/// `program_files_dirs` entry `exists` reports.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn windows_tool_roots(
    system_root: &str,
    program_files_dirs: &[&str],
    exists: impl Fn(&str) -> bool,
) -> Vec<String> {
    let system_root = if system_root.is_empty() {
        r"C:\Windows"
    } else {
        system_root
    };
    let mut roots = vec![
        format!(r"{system_root}\System32"),
        format!(r"{system_root}\System32\WindowsPowerShell\v1.0"),
    ];
    for pf in program_files_dirs {
        let git_bin = format!(r"{pf}\Git\usr\bin");
        if exists(&git_bin) {
            roots.push(git_bin);
        }
    }
    roots
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
    /// sigil must refuse.
    fn homeless_ctx(cwd: &str) -> FreezeCtx {
        FreezeCtx {
            home: None,
            cwd: cwd.into(),
        }
    }

    fn frozen(paths: &[&str], ctx: &FreezeCtx) -> Result<Vec<String>, PolicyError> {
        ctx.paths(&paths.iter().map(ToString::to_string).collect::<Vec<_>>())
            .map(|v| v.iter().map(|p| p.as_str().to_string()).collect())
    }

    // Unix-only: `PathBuf::join` yields `\` separators on Windows.
    #[cfg(unix)]
    #[test]
    fn freeze_expands_cwd_sigil() {
        let paths = frozen(&["cwd:", "cwd:/src"], &ctx("/h", "/work/proj")).unwrap();
        assert_eq!(
            paths,
            vec!["/work/proj".to_string(), "/work/proj/src".to_string()]
        );
    }

    #[test]
    fn freeze_expands_tempdir_sigil() {
        let paths = frozen(&["tempdir:", "tempdir:/scratch"], &ctx("/h", "/cwd")).unwrap();
        // macOS `TMPDIR` ends in `/`, which folding strips, so compare
        // against the same kernel rather than a literal.
        let temp = std::env::temp_dir();
        let fold = |p: &Path| fold_dots(p).to_string_lossy().into_owned();
        assert_eq!(paths[0], fold(&temp));
        assert_eq!(paths[1], fold(&temp.join("scratch")));
    }

    #[test]
    fn freeze_leaves_literal_paths_alone() {
        let paths = frozen(&["/tmp", "/etc/hosts"], &ctx("/h", "/cwd")).unwrap();
        // `fold_dots` rebuilds with the host separator, so the frozen form is
        // `\tmp` on Windows: compare against the same kernel.
        let fold = |s: &str| fold_dots(Path::new(s)).to_string_lossy().into_owned();
        assert_eq!(paths, vec![fold("/tmp"), fold("/etc/hosts")]);
    }

    /// The guard folds the whole prefix before comparing, so a `..` climb out
    /// of HOME is rejected at freeze rather than collapsing at match time.
    // Unix-only: the folded escape `/etc` is a Unix root.
    #[cfg(unix)]
    #[test]
    fn freeze_rejects_xdg_subpath_escaping_home() {
        let err = frozen(&["xdg:config/../../../../etc"], &ctx("/h", "/cwd"))
            .unwrap_err()
            .message;
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
            frozen(&["xdg:data"], &ctx(&home_str, "/cwd"))
                .unwrap_err()
                .message
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
            frozen(&["xdg:data"], &ctx(&home_str, "/cwd")).unwrap();
        });
    }

    /// A HOME of the bare root contains every XDG base, as the fs guard reads
    /// a prefix of `/`: the freeze asks its question, not a lexical twin.
    #[cfg(windows)]
    #[test]
    fn freeze_accepts_xdg_under_a_bare_root_home() {
        crate::test_env::with_var("XDG_DATA_HOME", None, || {
            frozen(&["xdg:data"], &ctx(r"\", r"C:\cwd")).unwrap();
        });
    }

    /// `Path::join` on a rooted suffix discards the base, so an `xdg:` token
    /// spelled with a double slash must not name the root.  Both halves of
    /// expansion answer alike.
    // Unix-only: Linux XDG defaults, and `/etc` is a Unix root.
    #[cfg(unix)]
    #[test]
    fn xdg_subpath_cannot_discard_its_base() {
        crate::test_env::with_var("XDG_CONFIG_HOME", None, || {
            let paths = frozen(&["xdg:config//etc"], &ctx("/h", "/cwd")).unwrap();
            assert_eq!(paths, vec!["/h/.config/etc".to_string()]);
            assert_eq!(
                expand_path_prefix("xdg:config//etc", Some("/h")),
                "/h/.config/etc"
            );
        });
    }

    /// Even a sigil-free literal is stored in the form the in-process guard matches against.
    // Unix-only: Unix path shapes.
    #[cfg(unix)]
    #[test]
    fn freeze_folds_dot_dot_in_literal() {
        let paths = frozen(&["/a/b/../c"], &ctx("/h", "/cwd")).unwrap();
        assert_eq!(paths, vec!["/a/c".to_string()]);
    }

    #[test]
    fn an_unknown_xdg_token_is_refused_with_the_known_kinds() {
        let err = frozen(&["xdg:cofnig"], &ctx("/h", "/cwd"))
            .unwrap_err()
            .message;
        assert!(err.contains("xdg:cofnig"), "got {err}");
        assert!(err.contains("config"), "should list known kinds: {err}");
    }

    /// A home-relative sigil has no answer where nothing binds `HOME`.  The
    /// freeze refuses and says which env var is missing; the runtime twin
    /// passes the entry through literally, a prefix matching nothing being the
    /// fail-closed reading on that side.  Neither fabricates `/.gitconfig`.
    #[test]
    fn freeze_rejects_home_relative_sigils_without_home() {
        for entry in ["~", "~/.gitconfig", "xdg:config/git"] {
            let err = frozen(&[entry], &homeless_ctx("/cwd")).unwrap_err().message;
            assert!(err.contains("HOME is unset"), "{entry}: {err}");
        }
        assert_eq!(expand_path_prefix("~/.gitconfig", None), "~/.gitconfig");
        assert_eq!(expand_path_prefix("xdg:config/git", None), "xdg:config/git");
    }

    #[test]
    fn unix_tool_roots_always_carries_usr_bin_and_bin() {
        let roots = unix_tool_roots(|_| false);
        assert_eq!(roots, vec!["/usr/bin".to_string(), "/bin".to_string()]);
    }

    #[test]
    fn unix_tool_roots_adds_present_homebrew_prefix_only() {
        let roots = unix_tool_roots(|p| p == "/opt/homebrew");
        assert_eq!(
            roots,
            vec![
                "/usr/bin".to_string(),
                "/bin".to_string(),
                "/opt/homebrew".to_string(),
            ]
        );
    }

    #[test]
    fn unix_tool_roots_adds_each_present_toolchain_root_only() {
        let toolchains = [
            "/usr/libexec",
            "/usr/lib/git-core",
            "/usr/lib/gcc",
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
        ];
        assert_eq!(
            unix_tool_roots(|p| toolchains.contains(&p)).split_at(2).1,
            toolchains
        );
        for present in toolchains {
            let roots = unix_tool_roots(|p| p == present);
            assert_eq!(roots, ["/usr/bin", "/bin", present], "{present}");
        }
    }

    /// No `cfg(windows)` on any of the Windows-shape tests: they run on the
    /// macOS and Linux CI hosts that never compile `system_tool_roots`' other
    /// half.
    #[test]
    fn windows_tool_roots_always_carries_system32_and_powershell() {
        let roots = windows_tool_roots(r"C:\Windows", &[], |_| false);
        assert_eq!(
            roots,
            vec![
                r"C:\Windows\System32".to_string(),
                r"C:\Windows\System32\WindowsPowerShell\v1.0".to_string(),
            ]
        );
    }

    #[test]
    fn windows_tool_roots_falls_back_when_system_root_unset() {
        let roots = windows_tool_roots("", &[], |_| false);
        assert!(roots[0].starts_with(r"C:\Windows"), "got {roots:?}");
    }

    #[test]
    fn windows_tool_roots_adds_git_for_windows_usr_bin_when_present() {
        let roots = windows_tool_roots(r"C:\Windows", &[r"C:\Program Files"], |p| {
            p == r"C:\Program Files\Git\usr\bin"
        });
        assert!(
            roots.contains(&r"C:\Program Files\Git\usr\bin".to_string()),
            "got {roots:?}"
        );
    }

    #[test]
    fn windows_tool_roots_omits_git_for_windows_when_absent() {
        let roots = windows_tool_roots(r"C:\Windows", &[r"C:\Program Files"], |_| false);
        assert!(!roots.iter().any(|r| r.contains("Git")), "got {roots:?}");
    }

    #[test]
    fn windows_tool_roots_checks_both_program_files_locations() {
        let roots = windows_tool_roots(
            r"C:\Windows",
            &[r"C:\Program Files", r"C:\Program Files (x86)"],
            |p| p == r"C:\Program Files (x86)\Git\usr\bin",
        );
        assert!(
            roots.contains(&r"C:\Program Files (x86)\Git\usr\bin".to_string()),
            "got {roots:?}"
        );
    }
}
