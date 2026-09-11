//! Stage 4 of path resolution: locating the *object* a resolved name denotes,
//! so the thing a gate authorises is the thing the kernel then operates on.
//!
//! A path-based gate judges a canonicalised string and the open that follows
//! re-walks the original one; whatever differs between those two walks — a
//! dangling link the canonicaliser could not follow, a directory swapped for a
//! symlink in between — is an object the gate never saw.  [`walk`] does one
//! walk: from the root, one component at a time through directory handles,
//! never letting the kernel follow a symlink, splicing each link it meets
//! into the remaining name itself.  Where it lands is [`Located::real`],
//! canonical by construction, and every operation a [`Located`] offers is
//! relative to the handle of its directory with `FollowSymlinks::No`.
//!
//! This is the one file that opens a model-named object, so the reviewed
//! syscall sites (`core/tests/syscall_sites.rs`) can be read off it: [`open_discard`] is
//! the sole exception, a device with no object to locate.

use cap_fs_ext::OpenOptionsFollowExt;
use cap_primitives::ambient_authority;
use cap_primitives::fs::{
    AccessModes, AccessType, FollowSymlinks, Metadata, OpenOptions, access, open, open_ambient_dir,
    open_dir_nofollow, read_base_dir, read_link_contents, remove_file, rename, stat,
};
use cap_primitives::time::SystemTime as CapTime;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use super::ResolvedPath;
use super::lex::fold_dots;

/// `SYMLOOP_MAX`'s customary value: a name still splicing past this is a cycle.
const MAX_HOPS: usize = 40;

/// An object named exactly: the handle of its directory, its name there, and
/// the symlink-free path the walk assembled.
///
/// The object need not exist — a name whose leaf is absent locates the place
/// a create will put it.
pub struct Located {
    dir: File,
    leaf: OsString,
    real: PathBuf,
}

/// What the object is.  One of these and no other, so an enum rather than a
/// row of bools that can contradict one another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Kind {
    /// The name ral's `list-dir` and `file-info` give this kind.
    pub fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

/// What a `stat` of the object found, when it exists.
///
/// `kind` is `Symlink` only for a [`Leaf::AsNamed`] locate; a resolving walk
/// has already followed the link, so what it stats is the target.  A
/// timestamp the filesystem does not record is `None`, not an epoch: the two
/// are different answers, and only the caller knows how to say so.
pub(crate) struct Stat {
    pub kind: Kind,
    pub(crate) len: u64,
    pub(crate) readonly: bool,
    pub(crate) mtime: Option<SystemTime>,
    pub(crate) atime: Option<SystemTime>,
    pub(crate) btime: Option<SystemTime>,
    #[cfg(unix)]
    pub(crate) mode: u32,
}

impl From<&Metadata> for Stat {
    fn from(m: &Metadata) -> Self {
        Self {
            // Symlink first: a nofollow stat of a link reports the link.
            kind: if m.file_type().is_symlink() {
                Kind::Symlink
            } else if m.is_dir() {
                Kind::Dir
            } else if m.is_file() {
                Kind::File
            } else {
                Kind::Other
            },
            len: m.len(),
            readonly: m.permissions().readonly(),
            mtime: m.modified().ok().map(CapTime::into_std),
            atime: m.accessed().ok().map(CapTime::into_std),
            btime: m.created().ok().map(CapTime::into_std),
            #[cfg(unix)]
            mode: {
                use cap_primitives::fs::PermissionsExt;
                m.permissions().mode() & 0o7777
            },
        }
    }
}

/// One entry of [`Located::read_dir`]: its name in the directory, and what
/// stating it without following found.
pub(crate) struct Entry {
    pub name: OsString,
    pub(crate) stat: Stat,
}

enum Step {
    Object(Located),
    Link(PathBuf),
}

/// What the walk does with a symlink at the *final* component.  Directory
/// components are always resolved; only the leaf is in question.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leaf {
    /// Follow it to what it points at.  What an open wants: `> link` writes
    /// through to the target, and the grant judges the target, so a link is
    /// never a way to smuggle a write past a deny.
    Resolve,
    /// Stop at the link itself.  What `exists`, `is-link` and `file-info`
    /// want: a dangling link exists, and a link is a link.
    AsNamed,
}

/// Locate `rp`, following symlinks by splicing rather than by the kernel.
///
/// # Errors
/// A missing or untraversable intermediate directory, a link cycle past
/// [`MAX_HOPS`], or the root itself, which is nobody's leaf.
pub(crate) fn walk(rp: &ResolvedPath, leaf: Leaf) -> io::Result<Located> {
    let mut path = rp.as_path().to_path_buf();
    for _ in 0..MAX_HOPS {
        match descend(&path, leaf)? {
            Step::Object(located) => return Ok(located),
            Step::Link(spliced) => path = spliced,
        }
    }
    Err(io::Error::other(LOOP_MESSAGE))
}

/// `ELOOP`'s wording; `io::ErrorKind::FilesystemLoop` is not yet stable.
const LOOP_MESSAGE: &str = "too many levels of symbolic links";

/// One pass from the root.  `Link` carries the whole name with the first
/// symlink met spliced in; the caller restarts from the root, so a target
/// that itself crosses links is handled by the same rule.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-descend] Opens the root and then each directory component as a search handle with FollowSymlinks::No, to reach the object a grant will judge. Path resolution, not the model's data I/O — the card belongs to the site that then opens the leaf."
)]
fn descend(path: &Path, leaf_mode: Leaf) -> io::Result<Step> {
    let mut comps = path.components().peekable();
    let mut real = PathBuf::new();
    while let Some(c @ (Component::Prefix(_) | Component::RootDir)) = comps.peek() {
        real.push(c.as_os_str());
        comps.next();
    }
    let names: Vec<&OsStr> = comps.map(Component::as_os_str).collect();
    let Some((leaf, dirs)) = names.split_last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the root directory is not a file",
        ));
    };
    let mut dir = open_ambient_dir(&real, ambient_authority())?;
    for (i, name) in dirs.iter().enumerate() {
        match open_search_dir(&dir, name) {
            Ok(next) => {
                dir = next;
                real.push(name);
            }
            Err(e) => {
                if !is_symlink(&dir, name) {
                    return Err(e);
                }
                return splice(&dir, &real, name, &names[i + 1..]).map(Step::Link);
            }
        }
    }
    if leaf_mode == Leaf::Resolve && is_symlink(&dir, leaf) {
        return splice(&dir, &real, leaf, &[]).map(Step::Link);
    }
    real.push(leaf);
    Ok(Step::Object(Located {
        dir,
        leaf: leaf.to_os_string(),
        real,
    }))
}

/// A search handle on `name` in `dir`: the right to resolve names through
/// it, not to read it — what the kernel's own resolver holds on an ancestor,
/// and so all a sandbox need grant one.  cap-primitives opens it `O_PATH`
/// where the platform has that; Apple's spelling is `O_SEARCH`, which it does
/// not know, so there it would fall back to a read handle.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-search] Opens one directory component of the walk as a search handle with FollowSymlinks::No. Path resolution, not the model's data I/O — the card belongs to the site that then opens the leaf."
)]
fn open_search_dir(dir: &File, name: &OsStr) -> io::Result<File> {
    #[cfg(target_os = "macos")]
    {
        use cap_primitives::fs::OpenOptionsExt;
        let mut opts = OpenOptions::new();
        opts.read(true)
            .custom_flags(libc::O_SEARCH)
            .follow(FollowSymlinks::No);
        open(dir, name.as_ref(), &opts)
    }
    #[cfg(not(target_os = "macos"))]
    open_dir_nofollow(dir, name.as_ref())
}

/// Absent counts as not a link: a missing leaf is a legitimate create target,
/// and a missing directory is reported by the open that just failed on it.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-link-probe] Stats one component to tell a symlink from an object, so the walk splices the link itself rather than letting the kernel follow it. A shape predicate, not turn-time model data I/O."
)]
fn is_symlink(dir: &File, name: &OsStr) -> bool {
    stat(dir, name.as_ref(), FollowSymlinks::No).is_ok_and(|m| m.is_symlink())
}

/// The name with `link` replaced by its target — anchored at the link's own
/// directory when relative — and `rest` re-appended, then folded.  Folding
/// here is sound because `real` holds no symlinks, so a lexical `..` is the
/// physical parent.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:walk-link-read] Reads a symlink's target to splice into the remaining name. Path resolution, not the model's data I/O."
)]
fn splice(dir: &File, real: &Path, link: &OsStr, rest: &[&OsStr]) -> io::Result<PathBuf> {
    let target = read_link_contents(dir, link.as_ref())?;
    let mut spliced = if target.is_absolute() {
        target
    } else {
        real.join(target)
    };
    spliced.extend(rest);
    Ok(fold_dots(&spliced))
}

fn nofollow(opts: &mut OpenOptions) -> &mut OpenOptions {
    opts.follow(FollowSymlinks::No)
}

impl Located {
    /// The symlink-free path of the object: what a gate judges and a card names.
    pub(crate) fn real(&self) -> &Path {
        &self.real
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "[surface:locate-open] The one open of a located object. Every caller surfaces it, each in its own way: a redirect's card is fused on by the frame that wrapped the locate — read recorded eagerly by install_stdin_redirect so it precedes what it feeds, write fired when the frame settles — while exarch's readers speak their own card and its editors emit a write event over a silent read. The atomic write's before-image and grep's per-file read ride the card of the operation that asked for them."
    )]
    fn open_leaf(&self, opts: &mut OpenOptions) -> io::Result<File> {
        open(&self.dir, self.leaf.as_ref(), nofollow(opts))
    }

    /// # Errors
    /// The open's, including `NotFound`.
    pub fn read(&self) -> io::Result<File> {
        self.open_leaf(OpenOptions::new().read(true))
    }

    /// Open for appending, creating an empty file if absent.
    ///
    /// # Errors
    /// The open's.
    pub(crate) fn append(&self) -> io::Result<File> {
        self.open_leaf(OpenOptions::new().create(true).append(true))
    }

    /// Open for writing from the start, creating or truncating.
    ///
    /// # Errors
    /// The open's.
    pub(crate) fn truncate(&self) -> io::Result<File> {
        self.open_leaf(OpenOptions::new().create(true).write(true).truncate(true))
    }

    /// `None` when the object does not exist.
    ///
    /// Never follows: the walk already settled that question, so a
    /// [`Leaf::Resolve`] locate stats what the link pointed at, and a
    /// [`Leaf::AsNamed`] one stats the link. `Kind::Symlink` is therefore
    /// reachable only through the latter.
    ///
    /// # Errors
    /// Any `stat` failure other than absence.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:locate-stat] Stats the located object: for the write site, to choose atomic against streaming semantics, to carry the mode onto the staged file and to size the before-image; for `exists`/`is-file`/`file-info`, as the predicate they are. A metadata read, never the model's turn-time data I/O — the write site's own card is its surface, and a predicate raises none."
    )]
    pub(crate) fn stat(&self) -> io::Result<Option<Stat>> {
        match stat(&self.dir, self.leaf.as_ref(), FollowSymlinks::No) {
            Ok(m) => Ok(Some(Stat::from(&m))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The object's entries, each stated without following, sorted by name so
    /// a listing does not depend on the order the kernel happens to hand back.
    ///
    /// An entry whose own stat fails is dropped: a listing reports what it
    /// could see, rather than failing whole because one entry vanished mid-walk.
    ///
    /// # Errors
    /// The directory open's, including `NotADirectory`.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:locate-read-dir] `list-dir`'s enumeration of the located directory, relative to its own handle. A listing predicate, not turn-time model data I/O — the caller still judges each entry against the live grant, and raises no card."
    )]
    pub(crate) fn read_dir(&self) -> io::Result<Vec<Entry>> {
        let dir = open_dir_nofollow(&self.dir, self.leaf.as_ref())?;
        let mut entries: Vec<Entry> = read_base_dir(&dir)?
            .filter_map(|e| {
                let e = e.ok()?;
                Some(Entry {
                    name: e.file_name(),
                    stat: Stat::from(&e.metadata().ok()?),
                })
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The target of the object as written in the link, not resolved.
    ///
    /// # Errors
    /// The readlink's, including `InvalidInput` when it is not a link.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:locate-read-link] `file-info`'s symlink target: reads the link's own contents as a metadata predicate. Not turn-time model data I/O, raises no surface card."
    )]
    pub(crate) fn read_link(&self) -> io::Result<PathBuf> {
        read_link_contents(&self.dir, self.leaf.as_ref())
    }

    /// `access(2)` against the real uid/gid — not `permissions().readonly()`,
    /// which ignores ownership, so another user's 0644 file would read as
    /// writable.  Follows, as `test -w` does.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:locate-access] The `is-writable` predicate's access(2) against the real uid/gid. A permission predicate, not turn-time model data I/O, raises no surface card."
    )]
    pub(crate) fn is_writable(&self) -> bool {
        access(
            &self.dir,
            self.leaf.as_ref(),
            AccessType::Access(AccessModes {
                readable: false,
                writable: true,
                executable: false,
            }),
            FollowSymlinks::Yes,
        )
        .is_ok()
    }

    /// A fresh, exclusively created, dot-hidden sibling for staging a write,
    /// and its name.  Random so concurrent writers in one directory never
    /// collide; a collision retries.
    ///
    /// # Errors
    /// The create's, other than `AlreadyExists`.
    #[allow(
        clippy::disallowed_methods,
        reason = "[surface:locate-stage] The atomic `>` staging create: a fresh exclusive sibling in the target's own directory, holding the write until the rename commits it. A sub-step of the write site; the write card is the operation's surface."
    )]
    pub(crate) fn create_sibling_tmp(&self) -> io::Result<(File, OsString)> {
        loop {
            let mut name = String::from(".");
            name.extend((0..16).map(|_| fastrand::alphanumeric()));
            name.push_str(".ral-write.tmp");
            let name = OsString::from(name);
            match open(
                &self.dir,
                name.as_ref(),
                nofollow(OpenOptions::new().create_new(true).write(true)),
            ) {
                Ok(file) => return Ok((file, name)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// # Errors
    /// The open's.
    #[allow(
        clippy::disallowed_methods,
        reason = "[surface:locate-staged-read] Reads the staged temp back to seed the write card's new side, before the rename commits it. A sub-step of the write site, not a separate model read."
    )]
    pub(crate) fn sibling_read(&self, name: &OsStr) -> io::Result<File> {
        open(
            &self.dir,
            name.as_ref(),
            nofollow(OpenOptions::new().read(true)),
        )
    }

    /// # Errors
    /// The open's.
    #[allow(
        clippy::disallowed_methods,
        reason = "[surface:locate-staged-write] Re-opens the staged temp for writing so its bytes can be flushed durable before the rename. A sub-step of the write site's commit; these opens carry written bytes to disk, they are not separate model reads."
    )]
    pub(crate) fn sibling_write(&self, name: &OsStr) -> io::Result<File> {
        open(
            &self.dir,
            name.as_ref(),
            nofollow(OpenOptions::new().write(true)),
        )
    }

    /// Rename the sibling `name` onto this object, atomically within the
    /// directory.
    ///
    /// # Errors
    /// The rename's.
    #[allow(
        clippy::disallowed_methods,
        reason = "[surface:locate-commit] The atomic `>` commit step: rename the staged sibling onto the target within the one directory handle. The write surface fires when the write settles, committed once this returns Ok."
    )]
    pub(crate) fn rename_sibling_over(&self, name: &OsStr) -> io::Result<()> {
        rename(&self.dir, name.as_ref(), &self.dir, self.leaf.as_ref())
    }

    /// # Errors
    /// The unlink's.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:locate-abandon] Reasoned-silent rollback of the atomic `>`: unlink the staged temp for a write that will not land. The aborted write card is the surface; this removal raises none of its own."
    )]
    pub(crate) fn remove_sibling(&self, name: &OsStr) -> io::Result<()> {
        remove_file(&self.dir, name.as_ref())
    }

    /// Flush the directory entries to disk, so a rename just made survives a
    /// crash.
    ///
    /// # Errors
    /// The fsync's; Windows has no directory flush and errors here.
    pub(crate) fn sync_dir(&self) -> io::Result<()> {
        self.dir.sync_all()
    }
}

/// The discard device, opened by name.
///
/// The one object this module opens without walking to it: exempt from the
/// walk and from the grant alike, since there is nothing to locate, and on
/// Windows `NUL` is a name the Win32 layer resolves anywhere rather than an
/// entry in any directory.
///
/// # Errors
/// The open's.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:discard-device] `/dev/null` / `NUL` opened by name: no bytes reach or leave the model, and no grant region can contain a device that is not a file."
)]
pub(crate) fn open_discard(rp: &ResolvedPath) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(rp.as_path())
}

#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build the tree the walk is asked about"
)]
mod tests {
    use super::*;
    use crate::path::Resolver;

    fn located(dir: &Path, rel: &str) -> io::Result<Located> {
        located_with(dir, rel, Leaf::Resolve)
    }

    fn located_with(dir: &Path, rel: &str, leaf: Leaf) -> io::Result<Located> {
        let resolver = Resolver {
            home: None,
            cwd: Some(dir),
        };
        walk(&resolver.resolve(rel), leaf)
    }

    /// The two leaf modes are the whole difference: `Resolve` walks through a
    /// terminal link to its target, `AsNamed` stops at the link itself, which
    /// is the only way `exists` can say a dangling link is there and `is-link`
    /// can say it is a link.
    #[test]
    fn as_named_stops_at_the_link_that_resolve_walks_through() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = root(&tmp);
        std::fs::write(dir.join("target"), b"body").unwrap();
        std::os::unix::fs::symlink("target", dir.join("link")).unwrap();

        let resolved = located(&dir, "link").unwrap();
        assert_eq!(resolved.real(), dir.join("target"));
        assert_eq!(resolved.stat().unwrap().unwrap().kind, Kind::File);

        let as_named = located_with(&dir, "link", Leaf::AsNamed).unwrap();
        assert_eq!(as_named.real(), dir.join("link"));
        assert_eq!(as_named.stat().unwrap().unwrap().kind, Kind::Symlink);
        assert_eq!(as_named.read_link().unwrap(), Path::new("target"));
    }

    /// A dangling link is absent to `Resolve` and present to `AsNamed` —
    /// `exists` answers the second question, not the first.
    #[test]
    fn a_dangling_link_is_absent_resolved_and_present_as_named() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = root(&tmp);
        std::os::unix::fs::symlink("gone", dir.join("dangling")).unwrap();

        assert!(located(&dir, "dangling").unwrap().stat().unwrap().is_none());
        let as_named = located_with(&dir, "dangling", Leaf::AsNamed).unwrap();
        assert_eq!(as_named.stat().unwrap().unwrap().kind, Kind::Symlink);
    }

    /// The temp root as the walk resolves it: macOS hands out `/var/folders/…`,
    /// a link to `/private/var/folders/…`, and following one is the point.
    fn root(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        std::fs::canonicalize(tmp.path()).expect("the temp root canonicalises")
    }

    #[test]
    fn a_plain_new_file_locates_where_its_name_says() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = root(&tmp);
        let loc = located(&dir, "fresh").unwrap();
        assert_eq!(loc.real(), dir.join("fresh"));
        assert!(loc.stat().unwrap().is_none());
    }

    /// The finding that motivated the walk: a link whose target does not
    /// exist must locate the *target*, so a gate judges where the bytes go.
    #[test]
    fn a_dangling_link_locates_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = root(&tmp);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::os::unix::fs::symlink("../outside/marker", dir.join("work/dangling")).unwrap();
        let loc = located(&dir, "work/dangling").unwrap();
        assert_eq!(loc.real(), dir.join("outside/marker"));
    }

    #[test]
    fn a_link_in_the_directory_chain_is_spliced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = root(&tmp);
        std::fs::create_dir_all(dir.join("top/deep")).unwrap();
        std::os::unix::fs::symlink("top/deep", dir.join("alias")).unwrap();
        let loc = located(&dir, "alias/secret").unwrap();
        assert_eq!(loc.real(), dir.join("top/deep/secret"));
    }

    /// The walk asks of an ancestor what the kernel's resolver asks — search,
    /// not read — so a `--x` directory, and a sandbox that admits ancestors as
    /// metadata only, let it through.
    #[test]
    fn a_search_only_ancestor_is_walked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = root(&tmp);
        std::fs::create_dir_all(dir.join("gate/inner")).unwrap();
        std::fs::write(dir.join("gate/inner/f"), b"body").unwrap();
        std::fs::set_permissions(dir.join("gate"), PermissionsExt::from_mode(0o111)).unwrap();
        let loc = located(&dir, "gate/inner/f");
        std::fs::set_permissions(dir.join("gate"), PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(loc.unwrap().stat().unwrap().unwrap().kind, Kind::File);
    }

    #[test]
    fn a_link_cycle_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("b", tmp.path().join("a")).unwrap();
        std::os::unix::fs::symlink("a", tmp.path().join("b")).unwrap();
        let Err(err) = located(tmp.path(), "a") else {
            panic!("a cycle must not locate");
        };
        assert_eq!(err.to_string(), LOOP_MESSAGE);
    }

    #[test]
    fn the_leaf_open_never_follows_a_link_that_appeared_later() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = located(tmp.path(), "victim").unwrap();
        std::os::unix::fs::symlink("elsewhere", tmp.path().join("victim")).unwrap();
        assert!(
            loc.append().is_err(),
            "a swapped-in leaf link must not be followed"
        );
        assert!(!tmp.path().join("elsewhere").exists());
    }
}
