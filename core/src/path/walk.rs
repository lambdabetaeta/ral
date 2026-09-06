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

use cap_fs_ext::OpenOptionsFollowExt;
use cap_primitives::ambient_authority;
use cap_primitives::fs::{
    FollowSymlinks, OpenOptions, open, open_ambient_dir, open_dir_nofollow, read_link_contents,
    remove_file, rename, stat,
};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

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

/// What a `stat` of the object found, when it exists.
pub struct Stat {
    pub is_file: bool,
    pub len: u64,
    #[cfg(unix)]
    pub mode: u32,
}

enum Step {
    Object(Located),
    Link(PathBuf),
}

/// Locate `rp`, following symlinks by splicing rather than by the kernel.
///
/// # Errors
/// A missing or untraversable intermediate directory, a link cycle past
/// [`MAX_HOPS`], or the root itself, which is nobody's leaf.
pub fn walk(rp: &ResolvedPath) -> io::Result<Located> {
    let mut path = rp.as_path().to_path_buf();
    for _ in 0..MAX_HOPS {
        match descend(&path)? {
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
fn descend(path: &Path) -> io::Result<Step> {
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
        match open_dir_nofollow(&dir, name.as_ref()) {
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
    if is_symlink(&dir, leaf) {
        return splice(&dir, &real, leaf, &[]).map(Step::Link);
    }
    real.push(leaf);
    Ok(Step::Object(Located {
        dir,
        leaf: leaf.to_os_string(),
        real,
    }))
}

/// Absent counts as not a link: a missing leaf is a legitimate create target,
/// and a missing directory is reported by the open that just failed on it.
fn is_symlink(dir: &File, name: &OsStr) -> bool {
    stat(dir, name.as_ref(), FollowSymlinks::No).is_ok_and(|m| m.is_symlink())
}

/// The name with `link` replaced by its target — anchored at the link's own
/// directory when relative — and `rest` re-appended, then folded.  Folding
/// here is sound because `real` holds no symlinks, so a lexical `..` is the
/// physical parent.
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
    pub fn real(&self) -> &Path {
        &self.real
    }

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
    pub fn append(&self) -> io::Result<File> {
        self.open_leaf(OpenOptions::new().create(true).append(true))
    }

    /// Open for writing from the start, creating or truncating.
    ///
    /// # Errors
    /// The open's.
    pub fn truncate(&self) -> io::Result<File> {
        self.open_leaf(OpenOptions::new().create(true).write(true).truncate(true))
    }

    /// `None` when the object does not exist.
    ///
    /// # Errors
    /// Any `stat` failure other than absence.
    pub fn stat(&self) -> io::Result<Option<Stat>> {
        match stat(&self.dir, self.leaf.as_ref(), FollowSymlinks::No) {
            Ok(m) => Ok(Some(Stat {
                is_file: m.is_file(),
                len: m.len(),
                #[cfg(unix)]
                mode: {
                    use cap_primitives::fs::PermissionsExt;
                    m.permissions().mode() & 0o7777
                },
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A fresh, exclusively created, dot-hidden sibling for staging a write,
    /// and its name.  Random so concurrent writers in one directory never
    /// collide; a collision retries.
    ///
    /// # Errors
    /// The create's, other than `AlreadyExists`.
    pub fn create_sibling_tmp(&self) -> io::Result<(File, OsString)> {
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
    pub fn sibling_read(&self, name: &OsStr) -> io::Result<File> {
        open(&self.dir, name.as_ref(), nofollow(OpenOptions::new().read(true)))
    }

    /// # Errors
    /// The open's.
    pub fn sibling_write(&self, name: &OsStr) -> io::Result<File> {
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
    pub fn rename_sibling_over(&self, name: &OsStr) -> io::Result<()> {
        rename(&self.dir, name.as_ref(), &self.dir, self.leaf.as_ref())
    }

    /// # Errors
    /// The unlink's.
    pub fn remove_sibling(&self, name: &OsStr) -> io::Result<()> {
        remove_file(&self.dir, name.as_ref())
    }

    /// Flush the directory entries to disk, so a rename just made survives a
    /// crash.
    ///
    /// # Errors
    /// The fsync's; Windows has no directory flush and errors here.
    pub fn sync_dir(&self) -> io::Result<()> {
        self.dir.sync_all()
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] fixtures build the tree the walk is asked about"
)]
mod tests {
    use super::*;
    use crate::path::Resolver;

    fn located(dir: &Path, rel: &str) -> io::Result<Located> {
        let resolver = Resolver {
            home: None,
            cwd: Some(dir),
        };
        walk(&resolver.resolve(rel))
    }

    #[test]
    fn a_plain_new_file_locates_where_its_name_says() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = located(tmp.path(), "fresh").unwrap();
        assert_eq!(loc.real(), tmp.path().join("fresh"));
        assert!(loc.stat().unwrap().is_none());
    }

    /// The finding that motivated the walk: a link whose target does not
    /// exist must locate the *target*, so a gate judges where the bytes go.
    #[test]
    fn a_dangling_link_locates_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("work")).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("../outside/marker", tmp.path().join("work/dangling")).unwrap();
        let loc = located(tmp.path(), "work/dangling").unwrap();
        assert_eq!(loc.real(), tmp.path().join("outside/marker"));
    }

    #[test]
    fn a_link_in_the_directory_chain_is_spliced() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("top/deep")).unwrap();
        std::os::unix::fs::symlink("top/deep", tmp.path().join("alias")).unwrap();
        let loc = located(tmp.path(), "alias/secret").unwrap();
        assert_eq!(loc.real(), tmp.path().join("top/deep/secret"));
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
        assert!(loc.append().is_err(), "a swapped-in leaf link must not be followed");
        assert!(!tmp.path().join("elsewhere").exists());
    }
}
