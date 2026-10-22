//! What each authority marker may and may not do, pinned from inside the
//! crate, where the `pub(crate)` doors are in reach.  The `pub` surface's
//! compile-fail cases are `core/tests/markers/`.
//!
//! A door is *absent* where a call on the handle finds no inherent method of
//! its name and falls through to [`Fallback`]'s, which answers [`Absent`]:
//! inherent methods win wherever their impl applies, so a call typed `Absent`
//! found none, and a call that finds the door stops this module compiling.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build the tree the handles are walked from, and the source scan reads this crate's own files"
)]

use super::super::*;
use crate::path::LexicalPath;

/// What a call answers when no door of that name was there.
struct Absent;

/// Every door the markers withhold, as a method no inherent one shadows.
trait Fallback {
    fn read(&self) -> Absent {
        Absent
    }
    fn stat(&self) -> Absent {
        Absent
    }
    fn read_link(&self) -> Absent {
        Absent
    }
    fn read_dir(&self) -> Absent {
        Absent
    }
    fn open_dir<T>(&self, _: T) -> Absent {
        Absent
    }
    fn locate<T>(&self, _: T, _: T) -> Absent {
        Absent
    }
    fn open_streaming<T>(&self, _: T) -> Absent {
        Absent
    }
    fn create_file<T>(&self, _: T) -> Absent {
        Absent
    }
    fn stage(&self) -> Absent {
        Absent
    }
    fn create_dir<T>(&self, _: T) -> Absent {
        Absent
    }
    fn leaf_fd(&self) -> Absent {
        Absent
    }
    fn reopen<T>(&self, _: T) -> Absent {
        Absent
    }
    fn is_local(&self) -> Absent {
        Absent
    }
}

impl<T> Fallback for T {}

/// A file and a directory under a fresh root, walked: the unjudged handles
/// the walk returns.
fn walked() -> (tempfile::TempDir, Located, Dir) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("f"), b"x").unwrap();
    let at = |path: &Path| LexicalPath::absolute(path).expect("a temp path is absolute");
    let located = walk(&at(&tmp.path().join("f")), LeafMode::Follow).unwrap();
    let dir = walk_dir(&at(tmp.path())).unwrap();
    (tmp, located, dir)
}

fn sub() -> EntryName {
    EntryName::parse(OsStr::new("sub")).expect("a name")
}

/// Nobody has vouched: the walk's product offers its trace and nothing more.
#[test]
fn an_unjudged_object_offers_its_trace_and_nothing_more() {
    let (_tmp, located, _) = walked();
    let _: &Trace = located.trace();
    let _: &RealPath = located.real();
    let _: Absent = located.read();
    let _: Absent = located.stat();
    let _: Absent = located.read_link();
    let _: Absent = located.open_streaming(());
    let _: Absent = located.stage();
    let _: Absent = located.create_file(());
    let _: Absent = located.create_dir(());
    let _: Absent = located.leaf_fd();
    let _: Absent = located.reopen(());
    let _: Absent = located.is_local();
}

#[test]
fn an_unjudged_directory_lists_nothing_and_states_nothing() {
    let (_tmp, _, dir) = walked();
    let _: &Trace = dir.trace();
    let _: &RealPath = dir.real();
    let _: Absent = dir.read_dir();
    let _: Absent = dir.stat();
    let _: Absent = dir.open_dir(());
    let _: Absent = dir.locate((), ());
    let _: Absent = dir.reopen(());
    let _: Absent = dir.is_local();
}

/// A read judgment buys bytes, metadata and a listing, and no write door.
#[test]
fn a_read_judged_object_reads_and_states_and_has_no_write_door() {
    let (_tmp, located, _) = walked();
    let located: Located<Judged<Reading>> = located.marked();
    let _: io::Result<File> = located.read();
    let _: Option<&Stat> = located.stat();
    let _: io::Result<PathBuf> = located.read_link();
    let _: Absent = located.open_streaming(());
    let _: Absent = located.stage();
    let _: Absent = located.create_file(());
    let _: Absent = located.create_dir(());
    let _: Absent = located.leaf_fd();
    let _: Absent = located.reopen(());
    let _: Absent = located.is_local();
}

/// A judged directory's children are unjudged: the region judges each on entry.
#[test]
fn a_read_judged_directory_descends_to_an_unjudged_one() {
    let (_tmp, _, dir) = walked();
    let dir: Dir<Judged<Reading>> = dir.marked();
    let _: io::Result<Listing<'_>> = dir.read_dir();
    let child: io::Result<Dir<Unjudged>> = dir.open_dir(&sub());
    assert!(child.is_ok());
    let _: Absent = dir.reopen(());
    let _: Absent = dir.is_local();
}

/// The sandbox side keeps its authority down a tree, and reads nothing.
#[test]
fn an_installing_directory_lists_and_descends_to_an_installing_one_and_reads_nothing() {
    let (_tmp, _, dir) = walked();
    let dir: Dir<Installing> = dir.marked();
    let _: io::Result<Listing<'_>> = dir.read_dir();
    let child: io::Result<Dir<Installing>> = dir.open_dir(&sub());
    assert!(child.is_ok());
    let _: Absent = dir.read();
}

#[test]
fn an_installing_object_states_and_reads_nothing() {
    let (_tmp, located, _) = walked();
    let located: Located<Installing> = located.marked();
    let _: Option<&Stat> = located.stat();
    let _: Absent = located.read();
    let _: Absent = located.open_streaming(());
    let _: Absent = located.stage();
}

/// Whether `T` is `AsFd`, as an associated constant, which an inherent
/// `impl` bounded by the trait overrides where the bound holds.
#[cfg(target_os = "linux")]
mod lends {
    use std::marker::PhantomData;
    use std::os::fd::AsFd;

    pub struct Is<T>(pub PhantomData<T>);

    pub trait Not {
        const AS_FD: bool = false;
    }

    impl<T> Not for Is<T> {}

    impl<T: AsFd> Is<T> {
        pub const AS_FD: bool = true;
    }
}

/// The raw handle leaves the type under `Installing` alone: an `O_PATH`
/// directory descriptor opens children, which an unjudged holder may not.
#[cfg(target_os = "linux")]
#[test]
fn only_an_installing_handle_lends_its_descriptor() {
    use lends::{Is, Not as _};
    use std::os::fd::BorrowedFd;
    macro_rules! lends {
        ($t:ty) => {
            std::hint::black_box(Is::<$t>::AS_FD)
        };
    }
    assert!(lends!(Dir<Installing>));
    assert!(!lends!(Dir<Unjudged>));
    assert!(!lends!(Dir<Judged<Reading>>));
    assert!(!lends!(Dir<Judged<Writing>>));

    let (_tmp, located, _) = walked();
    let located: Located<Installing> = located.marked();
    let _: Option<BorrowedFd<'_>> = located.leaf_fd();
}

/// Where the source of this crate lies.
fn source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// The files of this crate that make `token` where it is declared, other than
/// `home` and this scan (whose string literals spell the constructors),
/// relative to `src/`: lines of code, not comments.
fn made_outside(home: &str, token: &str) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(&source(), &mut files);
    files
        .into_iter()
        .filter(|file| !file.ends_with(home) && !file.ends_with("path/walk/tests/markers.rs"))
        .filter(|file| {
            std::fs::read_to_string(file).unwrap().lines().any(|line| {
                !line.trim_start().starts_with("//") && line.contains(&format!("{token}("))
            })
        })
        .map(|file| shown(file.strip_prefix(source()).unwrap()))
        .collect()
}

/// `Verdict` and `InstallToken` are tokens whose field only their own module
/// can make, so neither door can be forged from outside it; widening a field
/// would let any file write the constructor, which no compiler objects to.
#[test]
fn the_two_tokens_are_made_in_their_own_modules_alone() {
    let read = |file: &str| std::fs::read_to_string(source().join(file)).unwrap();
    assert!(
        read("capability/fs.rs").contains("pub(crate) struct Verdict<O: Op>(PhantomData<O>);"),
        "`Verdict`'s field must stay private to `capability/fs.rs`"
    );
    assert!(
        read("sandbox.rs").contains("pub(crate) struct InstallToken(());"),
        "`InstallToken`'s field must stay private to `sandbox.rs`"
    );
    assert_eq!(
        made_outside("capability/fs.rs", "Verdict"),
        Vec::<String>::new()
    );
    assert_eq!(
        made_outside("sandbox.rs", "InstallToken"),
        Vec::<String>::new()
    );
}
