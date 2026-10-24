//! The walk on a real filesystem with real links.

use super::super::*;
use super::{writable, writing};
use crate::path::{CwdPath, Resolver};
use crate::types::EnvVars;
use serde::de::DeserializeOwned;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

pub(super) fn spelled(path: &Path) -> LexicalPath {
    let cwd = CwdPath::new(PathBuf::from("/")).expect("/ is a working directory");
    Resolver::at(&EnvVars::new(), &cwd).anchor(path)
}

/// `path` walked, and the directory `path` entered, as these tests hold
/// them: retagged as judged for writing, since the tests judge nothing.
pub(super) fn located_with(path: &Path, mode: LeafMode) -> io::Result<Located<Judged<Writing>>> {
    walk(&spelled(path), mode).map(writing)
}

fn located(path: &Path) -> io::Result<Located<Judged<Writing>>> {
    located_with(path, LeafMode::Follow)
}

pub(super) fn entered(path: &Path) -> io::Result<Dir<Judged<Writing>>> {
    walk_dir(&spelled(path)).map(writable)
}

/// The temp root as the walk resolves it: macOS hands out `/var/folders/…`,
/// a link to `/private/var/folders/…`, and following one is the point.
pub(super) fn root(tmp: &tempfile::TempDir) -> PathBuf {
    std::fs::canonicalize(tmp.path()).expect("the temp root canonicalises")
}

/// The identity of the object at `path`, which is a directory or a file.
pub(super) fn id(path: &Path) -> ObjectId {
    ObjectId::of(&File::open(path).expect("the object is there")).expect("it has an identity")
}

pub(super) fn root_user() -> bool {
    rustix::process::geteuid().is_root()
}

fn json<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("it serialises")
}

fn decodes<T: DeserializeOwned>(value: serde_json::Value) -> bool {
    serde_json::from_value::<T>(value).is_ok()
}

pub(super) fn name(raw: &str) -> EntryName {
    EntryName::parse(OsStr::new(raw)).expect("a name")
}

/// The two leaf modes are the whole difference: `Follow` walks through a
/// terminal link to its target, `NoFollow` stops at the link itself, which is
/// the only way `exists` can say a dangling link is there and `is-link` can
/// say it is a link.
#[test]
fn no_follow_stops_at_the_link_that_follow_walks_through() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("target"), b"body").unwrap();
    symlink("target", dir.join("link")).unwrap();

    let followed = located(&dir.join("link")).unwrap();
    assert_eq!(followed.real().as_path(), dir.join("target"));
    assert_eq!(followed.stat().unwrap().kind, Kind::File);

    let named = located_with(&dir.join("link"), LeafMode::NoFollow).unwrap();
    assert_eq!(named.real().as_path(), dir.join("link"));
    assert_eq!(named.stat().unwrap().kind, Kind::Symlink);
    assert_eq!(named.read_link().unwrap(), Path::new("target"));
}

/// A link whose target does not exist must locate the *target*, so a guard
/// judges where the bytes go.
#[test]
fn a_dangling_link_locates_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("work")).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    symlink("../outside/marker", dir.join("work/dangling")).unwrap();
    let loc = located(&dir.join("work/dangling")).unwrap();
    assert_eq!(loc.real().as_path(), dir.join("outside/marker"));
}

#[test]
fn a_link_in_the_directory_chain_is_spliced() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("top/deep")).unwrap();
    symlink("top/deep", dir.join("alias")).unwrap();
    let loc = located(&dir.join("alias/secret")).unwrap();
    assert_eq!(loc.real().as_path(), dir.join("top/deep/secret"));
}

/// The walk asks of an ancestor what the kernel's resolver asks — search, not
/// read — so a `--x` directory, and a sandbox that admits ancestors as
/// metadata only, let it through.
#[test]
fn a_search_only_ancestor_is_walked() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("gate/inner")).unwrap();
    std::fs::write(dir.join("gate/inner/f"), b"body").unwrap();
    std::fs::set_permissions(dir.join("gate"), PermissionsExt::from_mode(0o111)).unwrap();
    let loc = located(&dir.join("gate/inner/f"));
    std::fs::set_permissions(dir.join("gate"), PermissionsExt::from_mode(0o755)).unwrap();
    assert_eq!(loc.unwrap().stat().unwrap().kind, Kind::File);
}

#[test]
fn a_link_cycle_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    symlink("b", tmp.path().join("a")).unwrap();
    symlink("a", tmp.path().join("b")).unwrap();
    let Err(err) = located(&tmp.path().join("a")) else {
        panic!("a cycle must not locate");
    };
    assert_eq!(err.to_string(), LOOP_MESSAGE);
    assert!(walk_lenient(&spelled(&tmp.path().join("a")), LeafMode::Follow).is_err());
}

/// Forty splices are the budget: a chain of forty links walks, one of
/// forty-one is a loop.
#[test]
fn the_splice_budget_is_forty() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("end"), b"").unwrap();
    symlink("end", dir.join("l0")).unwrap();
    for i in 1..=MAX_HOPS {
        symlink(format!("l{}", i - 1), dir.join(format!("l{i}"))).unwrap();
    }
    assert_eq!(
        located(&dir.join(format!("l{}", MAX_HOPS - 1)))
            .unwrap()
            .real()
            .as_path(),
        dir.join("end")
    );
    let Err(err) = located(&dir.join(format!("l{MAX_HOPS}"))) else {
        panic!("forty-one splices are a loop");
    };
    assert_eq!(err.to_string(), LOOP_MESSAGE);
}

/// Where a link is a redirection, the walk refuses it rather than splicing.
#[test]
fn a_walk_that_refuses_links_refuses_the_first_it_meets() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("real")).unwrap();
    symlink("real", dir.join("link")).unwrap();
    let refusing = |rel: &str| {
        finish(
            &dir.join(rel),
            Mode::Follow,
            OnMissing::Fail,
            Links::Refuse,
            |_: &Vetted<'_>| true,
        )
    };
    assert!(refusing("real").is_ok());
    for rel in ["link", "link/x"] {
        let Err(err) = refusing(rel) else {
            panic!("{rel} is a link");
        };
        assert_eq!(err.to_string(), LOOP_MESSAGE, "{rel}");
    }
}

#[test]
fn the_leaf_open_never_follows_a_link_that_appeared_later() {
    let tmp = tempfile::tempdir().unwrap();
    let loc = located(&tmp.path().join("victim")).unwrap();
    symlink("elsewhere", tmp.path().join("victim")).unwrap();
    assert!(
        loc.open_streaming(Streaming::Append).is_err(),
        "a swapped-in leaf link must not be followed"
    );
    assert!(!tmp.path().join("elsewhere").exists());
}

/// A `..` from a link target is the kernel's: physical, so it climbs out of
/// the link's target, not out of the link's place.
#[test]
fn a_dotdot_from_a_link_target_is_physical() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("x/y")).unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    symlink(dir.join("x/y"), dir.join("sub/lnk")).unwrap();
    symlink("sub/lnk/../f", dir.join("L")).unwrap();
    assert_eq!(
        located(&dir.join("L")).unwrap().real().as_path(),
        dir.join("x/f")
    );
}

/// Every restart is put to [`rooted`], the given name's and each spliced
/// one's alike: a name with no root is refused.
#[test]
fn a_walk_starts_only_from_a_root() {
    assert_eq!(
        rooted(Path::new("x")).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(rooted(Path::new("/x")).is_ok());
}

/// Physically: through a link, `..` climbs out of its target.
#[test]
fn a_trailing_dotdot_locates_the_directory_reached() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("top/deep")).unwrap();
    symlink("top/deep", dir.join("alias")).unwrap();
    for rel in ["top/deep/..", "alias/.."] {
        let loc = located(&dir.join(rel)).unwrap();
        assert_eq!(loc.real().as_path(), dir.join("top"), "{rel}");
        assert_eq!(loc.stat().unwrap().kind, Kind::Dir, "{rel}");
    }
}

#[test]
fn a_link_target_ending_in_a_separator_carries_the_requirement() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"body").unwrap();
    std::fs::create_dir(dir.join("d")).unwrap();
    symlink("f/", dir.join("to_file")).unwrap();
    symlink("d/", dir.join("to_dir")).unwrap();
    let Err(err) = located(&dir.join("to_file")) else {
        panic!("a link to `f/` names a directory");
    };
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    let loc = located(&dir.join("to_dir")).unwrap();
    assert_eq!(loc.real().as_path(), dir.join("d"));
    assert_eq!(loc.stat().unwrap().kind, Kind::Dir);
}

/// `alias -> d/` with a name still to come is `d` then the name, not a
/// requirement on the whole.
#[test]
fn a_link_to_a_directory_spelling_opens_the_file_beneath_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/file"), b"body").unwrap();
    symlink("d/", dir.join("alias")).unwrap();
    let loc = located(&dir.join("alias/file")).unwrap();
    assert_eq!(loc.real().as_path(), dir.join("d/file"));
    let mut body = String::new();
    loc.read().unwrap().read_to_string(&mut body).unwrap();
    assert_eq!(body, "body");
}

#[test]
fn a_dotdot_through_a_missing_directory_is_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    symlink("missing/../f", dir.join("L")).unwrap();
    let Err(err) = located(&dir.join("L")) else {
        panic!("the kernel gives ENOENT");
    };
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// A directory spelling is the directory itself, held as a node whose
/// container is the one beneath it: the root is its own container.
/// `walk_dir` of each is the same directory.
#[test]
fn a_directory_spelling_is_the_directory_entered() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let parent = dir.parent().unwrap().to_path_buf();
    std::fs::create_dir(dir.join("d")).unwrap();
    symlink("d/", dir.join("alias")).unwrap();
    let cases = [
        ("/".into(), "/".into(), "/".into()),
        (dir.join("d/"), dir.join("d"), dir.clone()),
        (dir.join("d/.."), dir.clone(), parent),
        (dir.join("alias"), dir.join("d"), dir.clone()),
    ];
    for (spelling, leaf, beneath) in cases {
        let loc = located(&spelling).unwrap();
        assert_eq!(loc.real().as_path(), leaf, "{spelling:?}");
        let Leaf::Dir { node, stat } = &loc.leaf else {
            panic!("{spelling:?} holds its directory");
        };
        assert_eq!(stat.kind, Kind::Dir);
        assert_eq!(
            ObjectId::of(&node.held.file).unwrap(),
            id(&leaf),
            "{spelling:?}"
        );
        assert_eq!(
            ObjectId::of(loc.dir()).unwrap(),
            id(&beneath),
            "{spelling:?}"
        );

        let held = entered(&spelling).unwrap();
        assert_eq!(held.trace(), loc.trace(), "{spelling:?}");
        assert_eq!(ObjectId::of(&held.node.held.file).unwrap(), id(&leaf));
        assert_eq!(held.stat().kind, Kind::Dir);
    }
    assert_eq!(
        entered(&dir.join("missing/")).err().unwrap().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn a_directory_spelling_of_a_file_or_a_missing_name_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"").unwrap();
    symlink("f", dir.join("to_f")).unwrap();
    for rel in ["f/", "to_f/", "to_f/."] {
        let Err(err) = located(&dir.join(rel)) else {
            panic!("{rel} is no directory");
        };
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{rel}");
    }
    let Err(err) = located(&dir.join("missing/")) else {
        panic!("missing/ is nothing");
    };
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    let Err(err) = entered(&dir.join("f")) else {
        panic!("a file is not entered");
    };
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
}

/// The transcript has the root's identity and one step per component.
#[test]
fn a_trace_has_the_roots_identity_and_one_step_per_component() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::fs::write(dir.join("a/b/f"), b"").unwrap();
    let located = located(&dir.join("a/b/f")).unwrap();
    let trace = located.trace();
    assert_eq!(trace.root_id, id(Path::new("/")));
    let depth = dir.components().count() - 1;
    let steps = trace.steps();
    assert_eq!(steps.len(), depth + 3);
    assert_eq!(trace.tail(), []);
    let mut at = PathBuf::from("/");
    for step in steps {
        at.push(step.name().as_os_str());
        assert_eq!(step.id(), id(&at), "{at:?}");
    }
    let kinds: Vec<Kind> = steps.iter().map(Step::kind).collect();
    assert!(kinds[..kinds.len() - 1].iter().all(|k| *k == Kind::Dir));
    assert_eq!(kinds.last(), Some(&Kind::File));
    assert_eq!(trace.kind(), Some(Kind::File));
    assert_eq!(trace.leaf_id(), Some(id(&dir.join("a/b/f"))));
}

/// A `..` in a link target pops one step: the transcript has no `..`.
#[test]
fn a_dotdot_in_a_link_target_pops_one_step() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::fs::write(dir.join("a/f"), b"").unwrap();
    symlink("../f", dir.join("a/b/l")).unwrap();
    let located = located(&dir.join("a/b/l")).unwrap();
    let plain = walk(&spelled(&dir.join("a/f")), LeafMode::Follow).unwrap();
    assert_eq!(located.trace().steps(), plain.trace().steps());
    assert_eq!(located.trace().tail(), plain.trace().tail());
    assert_eq!(located.real().as_path(), dir.join("a/f"));
}

/// What a walk passes besides its steps is in its trace, by position: each
/// link whose target it read, a chain's middle one too, and each directory
/// a `..` climbed out of.
#[test]
fn a_trace_keeps_what_the_walk_passed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    for made in ["sub", "real", "d"] {
        std::fs::create_dir(dir.join(made)).unwrap();
    }
    symlink("sub/../real", dir.join("middle")).unwrap();
    symlink(dir.join("middle"), dir.join("link")).unwrap();
    symlink("../real", dir.join("d/up")).unwrap();
    let passed = |trace: &Trace| -> Vec<PathBuf> {
        (trace.passed().iter())
            .map(|p| p.as_path().to_path_buf())
            .collect()
    };

    let chain = located(&dir.join("link")).unwrap();
    assert_eq!(chain.real().as_path(), dir.join("real"));
    assert_eq!(
        passed(chain.trace()),
        [dir.join("link"), dir.join("middle"), dir.join("sub")]
    );
    assert_eq!(
        serde_json::from_value::<Trace>(json(chain.trace())).unwrap(),
        *chain.trace()
    );

    assert_eq!(located(Path::new("/..")).unwrap().trace().passed(), []);

    let beneath = entered(&dir.join("d"))
        .unwrap()
        .locate(&name("up"), LeafMode::Follow)
        .unwrap();
    assert_eq!(beneath.real().as_path(), dir.join("real"));
    assert_eq!(passed(beneath.trace()), [dir.join("d/up"), dir.join("d")]);
}

/// A missing leaf has a one-name tail and a trace without a kind.
#[test]
fn a_missing_leaf_has_a_one_name_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let located = located(&dir.join("fresh")).unwrap();
    assert_eq!(located.trace().tail(), [name("fresh")]);
    assert_eq!(located.trace().kind(), None);
    assert_eq!(located.trace().leaf_id(), None);
    assert!(located.stat().is_none());
}

/// A lenient walk of a spelling whose last two names are missing has a
/// two-name tail and two fewer steps than components.
#[test]
fn a_lenient_walk_keeps_the_missing_names_as_a_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let strict = located(&dir).unwrap();
    let trace = walk_lenient(&spelled(&dir.join("one/two")), LeafMode::Follow).unwrap();
    assert_eq!(trace.tail(), [name("one"), name("two")]);
    assert_eq!(trace.steps().len(), strict.trace().steps().len());
    assert_eq!(trace.real().as_path(), dir.join("one/two"));
    assert_eq!(trace.kind(), None);
    let folded = walk_lenient(&spelled(&dir.join("one/../two")), LeafMode::Follow).unwrap();
    assert_eq!(folded.tail(), [name("two")]);
}

/// A lenient walk whose tail folds away resumes from the directory it holds:
/// `missing/../x` is walked to `x`, a step with its identity, the trace a
/// walk of `x` makes.
#[test]
fn a_lenient_walk_resumes_where_its_tail_folds_away() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("x"), b"body").unwrap();
    let trace = walk_lenient(&spelled(&dir.join("missing/../x")), LeafMode::Follow).unwrap();
    assert_eq!(trace.tail(), []);
    assert_eq!(trace.steps().last().unwrap().name(), &name("x"));
    assert_eq!(trace.leaf_id(), Some(id(&dir.join("x"))));
    assert_eq!(trace.kind(), Some(Kind::File));
    let direct = walk_lenient(&spelled(&dir.join("x")), LeafMode::Follow).unwrap();
    assert_eq!(trace, direct);
}

/// The names a resumed walk takes are walked as any are: a link among them is
/// followed or not by the walk's mode.
#[test]
fn a_resumed_lenient_walk_treats_a_link_by_its_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("target"), b"body").unwrap();
    symlink("target", dir.join("x")).unwrap();
    let walked = |rel: &str, mode| walk_lenient(&spelled(&dir.join(rel)), mode).unwrap();
    let followed = walked("missing/../x", LeafMode::Follow);
    assert_eq!(followed.real().as_path(), dir.join("target"));
    assert_eq!(followed.leaf_id(), Some(id(&dir.join("target"))));
    let named = walked("missing/../x", LeafMode::NoFollow);
    assert_eq!(named.real().as_path(), dir.join("x"));
    assert_eq!(named.kind(), Some(Kind::Symlink));
    for mode in [LeafMode::Follow, LeafMode::NoFollow] {
        assert_eq!(walked("missing/../x", mode), walked("x", mode));
    }
}

/// A lenient spelling that folds away to its last name names the directory
/// the walk resumed in, entered: an object, so no tail.
#[test]
fn a_lenient_walk_folded_away_to_its_end_enters_the_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("w")).unwrap();
    for (rel, at) in [
        ("w/missing/..", dir.join("w")),
        ("w/missing/../..", dir.clone()),
    ] {
        let located = finish(
            &dir.join(rel),
            Mode::Follow,
            OnMissing::Stop,
            Links::Splice,
            |_| true,
        )
        .unwrap();
        assert_eq!(located.trace().tail(), [], "{rel}");
        assert_eq!(located.real().as_path(), at, "{rel}");
        assert_eq!(located.trace().leaf_id(), Some(id(&at)), "{rel}");
        assert!(located.into_dir().is_ok(), "{rel}");
    }
}

/// A lenient walk whose tail folds away resumes from the directory it holds,
/// so a `..` past it climbs that directory: `w/l`, a link to
/// `missing/../../secret`, is fixed at `secret` beside `w`, the steps and tail
/// the walk makes once `missing` is there.
#[test]
fn a_lenient_dotdot_past_its_tail_climbs_the_held_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("w")).unwrap();
    symlink("missing/../../secret", dir.join("w/l")).unwrap();
    let walked = || walk_lenient(&spelled(&dir.join("w/l")), LeafMode::Follow).unwrap();
    let trace = walked();
    assert_eq!(trace.tail(), [name("secret")]);
    assert_eq!(trace.real().as_path(), dir.join("secret"));
    std::fs::create_dir(dir.join("w/missing")).unwrap();
    let there = walked();
    assert_eq!(trace.steps(), there.steps());
    assert_eq!(trace.tail(), there.tail());
    assert_eq!(trace.real(), there.real());
}

/// A lenient walk ends its steps at a name it cannot enter and keeps the
/// rest as the tail: a directory that may be walked to and not searched.
#[test]
fn a_lenient_walk_through_an_unreadable_directory_ends_its_steps_there() {
    if root_user() {
        eprintln!("skip: root searches every directory, so none is unreadable");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("closed/inner")).unwrap();
    std::fs::set_permissions(dir.join("closed"), PermissionsExt::from_mode(0o000)).unwrap();
    let trace = walk_lenient(&spelled(&dir.join("closed/inner/f")), LeafMode::Follow);
    std::fs::set_permissions(dir.join("closed"), PermissionsExt::from_mode(0o755)).unwrap();
    let trace = trace.unwrap();
    assert_eq!(trace.tail(), [name("inner"), name("f")]);
    assert_eq!(trace.steps().last().unwrap().name(), &name("closed"));
    assert_eq!(trace.real().as_path(), dir.join("closed/inner/f"));
}

/// A directory has a kind, and so has a link to one under `Follow`; a missing
/// tail has none; a present root has its own identity.
#[test]
fn a_lenient_trace_knows_its_kind_and_a_root_its_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    symlink("d", dir.join("to_d")).unwrap();
    let kind = |rel: &str, mode| walk_lenient(&spelled(&dir.join(rel)), mode).unwrap().kind();
    assert_eq!(kind("d", LeafMode::Follow), Some(Kind::Dir));
    assert_eq!(kind("to_d", LeafMode::Follow), Some(Kind::Dir));
    assert_eq!(kind("to_d", LeafMode::NoFollow), Some(Kind::Symlink));
    assert_eq!(kind("missing", LeafMode::Follow), None);
    let root_trace = walk_lenient(&spelled(Path::new("/")), LeafMode::Follow).unwrap();
    assert_eq!(root_trace.leaf_id(), Some(id(Path::new("/"))));
    assert_eq!(root_trace.kind(), Some(Kind::Dir));
}

/// A trace read off the wire is one the walk could have made: `real` is
/// recomputed, a step past a file is refused, and so is a tail past one.
#[test]
fn a_trace_decodes_checked_and_recomputed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/f"), b"").unwrap();
    let walked = located(&dir.join("d/f")).unwrap();
    let trace = walked.trace();
    let wire = json(trace);
    assert_eq!(
        serde_json::from_value::<Trace>(wire.clone()).unwrap(),
        *trace
    );

    let mut lying = wire.clone();
    lying["real"] = json(located(&dir.join("d")).unwrap().real());
    assert!(!decodes::<Trace>(lying), "a `real` the steps do not make");

    let [.., dstep, fstep] = trace.steps() else {
        panic!("a walk of d/f has steps");
    };
    let root_id = trace.root_id;
    let made = |steps: &[Step], tail: &[EntryName]| {
        json(&Trace::new(
            trace.root.clone(),
            root_id,
            trace.steps()[..trace.steps().len() - 2]
                .iter()
                .chain(steps)
                .cloned()
                .collect(),
            tail.to_vec(),
            Vec::new(),
        ))
    };
    assert!(decodes::<Trace>(made(&[dstep.clone(), fstep.clone()], &[])));
    assert!(
        !decodes::<Trace>(made(&[fstep.clone(), dstep.clone()], &[])),
        "a step beneath a file"
    );
    assert!(
        !decodes::<Trace>(made(&[dstep.clone(), fstep.clone()], &[name("more")])),
        "a tail beneath a file"
    );
    assert!(decodes::<Trace>(made(
        std::slice::from_ref(dstep),
        &[name("more")]
    )));

    let mut unnamed = wire;
    unnamed["steps"][0]["name"] = json(&OsString::from("a/b"));
    assert!(!decodes::<Trace>(unnamed), "a step named by a path");
}

#[test]
fn an_entry_name_is_one_normal_component() {
    for refused in ["", ".", "..", "a/b", "/a", "a/", "/"] {
        assert!(
            EntryName::parse(OsStr::new(refused)).is_err(),
            "{refused:?} is no entry name"
        );
        assert!(!decodes::<EntryName>(json(&OsString::from(refused))));
    }
    for named in ["a", ".hidden", "..a", "a b", "a\\b", "a:b"] {
        let parsed = EntryName::parse(OsStr::new(named)).unwrap();
        assert_eq!(parsed.as_os_str(), named);
        assert_eq!(
            serde_json::from_value::<EntryName>(json(&parsed)).unwrap(),
            parsed
        );
    }
    let refusal = EntryName::parse(OsStr::new("a/b")).unwrap_err().to_string();
    assert!(refusal.contains("separator"), "{refusal}");
}

#[test]
fn a_fresh_name_is_a_prefix_and_sixteen_alphanumerics() {
    let fresh = EntryName::fresh(".ral-write-").unwrap();
    let rest = fresh
        .as_os_str()
        .to_str()
        .unwrap()
        .strip_prefix(".ral-write-")
        .unwrap()
        .to_owned();
    assert_eq!(rest.len(), 16);
    assert!(rest.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(fresh, EntryName::fresh(".ral-write-").unwrap());
    assert!(EntryName::fresh("a/b").is_err());
    assert!(EntryName::fresh("../").is_err());
}

#[test]
fn a_root_name_is_a_root_and_nothing_more() {
    let root_of = |raw: &str| serde_json::from_value::<RootName>(json(&OsString::from(raw)));
    assert!(root_of("/").is_ok());
    for refused in ["", "x", "/a", "a/b", "."] {
        assert!(root_of(refused).is_err(), "{refused:?} is no root");
    }
    assert_eq!(json(&root_of("/").unwrap()), json(&OsString::from("/")));
}

/// A position is absolute, spelled as the walk spells one, and without `.`
/// or `..`; a non-Unicode one crosses the wire whole.
#[test]
fn a_real_path_decodes_checked_and_whole() {
    let position = |raw: &[u8]| {
        let raw = OsStr::from_bytes(raw).to_owned();
        serde_json::from_value::<RealPath>(json(&raw))
    };
    assert!(position(b"/").is_ok());
    assert!(position(b"/a/b").is_ok());
    for refused in ["", "a", "a/b", "/a/../b", "/a/./b", "//a", "/a/", "/.."] {
        assert!(
            position(refused.as_bytes()).is_err(),
            "{refused:?} is no position"
        );
    }
    let raw = position(b"/opt/\xFFtool").unwrap();
    let back = serde_json::from_value::<RealPath>(json(&raw)).unwrap();
    assert_eq!(back, raw);
    assert_eq!(back.as_path().as_os_str().as_bytes(), b"/opt/\xFFtool");
    assert_ne!(back, position("/opt/\u{FFFD}tool".as_bytes()).unwrap());
}

/// An identity read from a handle is the one a by-name `stat` reads.
#[cfg(target_os = "linux")]
#[test]
fn an_identity_agrees_across_handle_and_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"").unwrap();
    let file = File::open(dir.join("f")).unwrap();
    let held = File::open(&dir).unwrap();
    let named = rustix::fs::statat(&held, "f", rustix::fs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
    assert_eq!(ObjectId::of(&file).unwrap(), ObjectId::from(&named));
    assert_eq!(
        ObjectId::of(&file).unwrap(),
        host::seen_at(&held, OsStr::new("f")).unwrap().id
    );
    let meta = file.metadata().unwrap();
    assert_eq!(host::seen(&file).unwrap().stat.len, meta.len());
    assert_eq!(host::seen(&file).unwrap().stat.mode, meta.mode() & 0o7777);
}

/// A walk through an untriggered autofs mount point enters it: the step asks
/// for a directory first, which triggers the automount, where a plain `O_PATH`
/// open would answer the empty directory the mount lies over.  Run in a mount
/// namespace of its own where `automount` is on `PATH` and the user may make
/// one; the namespace re-runs this test with `RAL_TEST_AUTOFS` set.
#[cfg(target_os = "linux")]
#[test]
fn the_linux_walk_enters_an_untriggered_autofs_mount_point() {
    use std::process::Command;
    const ME: &str = "the_linux_walk_enters_an_untriggered_autofs_mount_point";
    if let Some(mounts) = std::env::var_os("RAL_TEST_AUTOFS") {
        let walked = located(&Path::new(&mounts).join("k")).unwrap();
        assert_eq!(walked.stat().unwrap().kind, Kind::Dir);
        return;
    }
    let on_path = |tool: &str| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
    };
    let (Some(automount), Some(_)) = (on_path("automount"), on_path("unshare")) else {
        eprintln!(
            "skip: `automount` and `unshare` are not both on PATH, and an untriggered autofs mount point needs the daemon"
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let mounts = dir.join("mnt");
    std::fs::create_dir(&mounts).unwrap();
    std::fs::write(
        dir.join("auto.master"),
        format!("{} {}\n", mounts.display(), dir.join("auto.map").display()),
    )
    .unwrap();
    std::fs::write(dir.join("auto.map"), "k -fstype=tmpfs :tmpfs\n").unwrap();
    let script = "\"$AUTOMOUNT\" -f -p \"$DIR/pid\" \"$DIR/auto.master\" & daemon=$!; \
                  trap 'kill $daemon' EXIT; \
                  for _ in 1 2 3 4 5 6 7 8 9 10; do mountpoint -q \"$DIR/mnt\" && break; sleep 0.2; done; \
                  \"$TEST\" --exact \"$NAME\" --nocapture";
    let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    let status = Command::new("unshare")
        .args(["--mount", "--propagation", "private", "sh", "-c", script])
        .env("RAL_TEST_AUTOFS", &mounts)
        .env("AUTOMOUNT", automount)
        .env("DIR", &dir)
        .env("TEST", std::env::current_exe().unwrap())
        .env("NAME", format!("{module}::{ME}"))
        .status();
    match status.map(|s| s.code()) {
        Ok(Some(0)) => {}
        Ok(Some(101)) => panic!("the walk did not enter the autofs mount point"),
        _ => eprintln!("skip: no mount namespace in which `automount` can run is available here"),
    }
}

/// A link is read through the handle the walk opened on it: replaced at its
/// name afterwards, it still answers what it said when opened.
#[test]
fn a_links_target_is_read_through_the_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    symlink("first", dir.join("l")).unwrap();
    let opened = located_with(&dir.join("l"), LeafMode::NoFollow).unwrap();
    std::fs::remove_file(dir.join("l")).unwrap();
    symlink("second", dir.join("l")).unwrap();
    assert_eq!(opened.read_link().unwrap(), Path::new("first"));
    let fresh = located_with(&dir.join("l"), LeafMode::NoFollow).unwrap();
    assert_eq!(fresh.read_link().unwrap(), Path::new("second"));
}

#[test]
fn only_a_link_has_a_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"").unwrap();
    let file = located(&dir.join("f")).unwrap();
    assert_eq!(
        file.read_link().unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let missing = located(&dir.join("gone")).unwrap();
    assert_eq!(
        missing.read_link().unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

/// A relative target continues from the directory the walk holds, not from
/// where that directory's name leads now.
#[test]
fn a_relative_target_continues_from_the_held_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("d/original")).unwrap();
    symlink("original", dir.join("d/l")).unwrap();
    let held = entered(&dir.join("d")).unwrap();
    let wanted = id(&dir.join("d/original"));

    std::fs::rename(dir.join("d"), dir.join("moved")).unwrap();
    std::fs::create_dir_all(dir.join("d/other")).unwrap();
    symlink("other", dir.join("d/l")).unwrap();

    let entered = held.open_dir_via_link(&name("l")).unwrap();
    assert_eq!(ObjectId::of(&entered.node.held.file).unwrap(), wanted);
    assert_eq!(entered.trace().real().as_path(), dir.join("d/original"));
    assert_eq!(writable(entered).stat().kind, Kind::Dir);
}

/// A `..` in a target climbs the directories the walk opened above the
/// directory, wherever the caller came from: `up` and `far` both land on
/// `a/c`, from a `Dir` entered beneath `a` and from one walked afresh.
#[test]
fn a_dotdot_in_a_target_climbs_the_directories_the_walk_opened() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::fs::create_dir(dir.join("a/c")).unwrap();
    symlink("../c", dir.join("a/b/up")).unwrap();
    symlink("../../a/c", dir.join("a/b/far")).unwrap();
    let below = writable(
        entered(&dir.join("a"))
            .unwrap()
            .open_dir(&name("b"))
            .unwrap(),
    );

    let up = below.open_dir_via_link(&name("up")).unwrap();
    assert_eq!(up.real().as_path(), dir.join("a/c"));
    assert_eq!(
        ObjectId::of(&up.node.held.file).unwrap(),
        id(&dir.join("a/c"))
    );
    let far = below.open_dir_via_link(&name("far")).unwrap();
    assert_eq!(far.real().as_path(), dir.join("a/c"));

    let fresh = writable(walk_dir(&spelled(&dir.join("a/b"))).unwrap());
    let up = fresh.open_dir_via_link(&name("up")).unwrap();
    assert_eq!(up.real().as_path(), dir.join("a/c"));
}

/// A `..` climbs the directory the walk holds, not the name it was reached
/// by: after `a` is renamed and another `a/c` made, `up` is still the
/// original.
#[test]
fn a_dotdot_climbs_the_held_directory_not_the_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::fs::create_dir_all(dir.join("a/c")).unwrap();
    std::fs::write(dir.join("a/c/t"), b"").unwrap();
    symlink("../c", dir.join("a/b/up")).unwrap();
    let below = writable(
        entered(&dir.join("a"))
            .unwrap()
            .open_dir(&name("b"))
            .unwrap(),
    );

    std::fs::rename(dir.join("a"), dir.join("moved")).unwrap();
    std::fs::create_dir_all(dir.join("a/c")).unwrap();

    let up = below.open_dir_via_link(&name("up")).unwrap();
    assert_eq!(
        ObjectId::of(&up.node.held.file).unwrap(),
        id(&dir.join("moved/c"))
    );
    assert_ne!(
        ObjectId::of(&up.node.held.file).unwrap(),
        id(&dir.join("a/c"))
    );
}

/// A link a walk follows to a directory continues its `..` from where the
/// link landed, not from the names it was reached by: finding `tmp/a/c`, not
/// `tmp/c`, whether the walk is spliced or a `Dir` of its own.
#[test]
fn a_followed_link_splices_dotdot_from_where_it_landed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::fs::create_dir_all(dir.join("a/c")).unwrap();
    std::fs::write(dir.join("a/c/t"), b"").unwrap();
    std::fs::create_dir_all(dir.join("c")).unwrap();
    std::fs::write(dir.join("c/wrong"), b"").unwrap();
    symlink(dir.join("a/b"), dir.join("link")).unwrap();
    symlink("../c", dir.join("a/b/next")).unwrap();
    let via = writable(
        entered(&dir)
            .unwrap()
            .open_dir_via_link(&name("link"))
            .unwrap(),
    );
    assert_eq!(via.real().as_path(), dir.join("a/b"));

    let opened = via.open_dir_via_link(&name("next")).unwrap();
    assert_eq!(opened.real().as_path(), dir.join("a/c"));
    let located = via.locate(&name("next"), LeafMode::Follow).unwrap();
    assert_eq!(located.real().as_path(), dir.join("a/c"));
    assert_eq!(located.observed().unwrap().kind, Kind::Dir);
    for found in [
        ObjectId::of(&opened.node.held.file).unwrap(),
        located.trace().last_id(),
    ] {
        assert_ne!(found, id(&dir.join("c")));
    }
}

/// A child link whose target climbs resolves through the directory's own
/// ancestry, whichever way the directory came to be held.
#[test]
fn a_child_link_climbs_the_directorys_own_ancestry() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("rules")).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    symlink("../rules", dir.join("sub/link")).unwrap();
    let link = name("link");
    let held = [
        entered(&dir.join("sub")).unwrap(),
        writable(entered(&dir).unwrap().open_dir(&name("sub")).unwrap()),
        located_with(&dir.join("sub"), LeafMode::NoFollow)
            .unwrap()
            .into_dir()
            .ok()
            .unwrap(),
    ];
    for sub in &held {
        let found = sub.locate(&link, LeafMode::Follow).unwrap();
        assert_eq!(found.real().as_path(), dir.join("rules"));
        let kept = sub.locate(&link, LeafMode::NoFollow).unwrap();
        assert_eq!(kept.observed().unwrap().kind, Kind::Symlink);
    }
}

/// A `..` past the root stays at the root.
#[test]
fn a_dotdot_past_the_root_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    symlink("../".repeat(32) + "etc", dir.join("up")).unwrap();
    let up = entered(&dir)
        .unwrap()
        .open_dir_via_link(&name("up"))
        .unwrap();
    assert_eq!(
        up.real().as_path(),
        Path::new("/etc").canonicalize().unwrap()
    );
}

#[test]
fn open_dir_enters_a_directory_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("f"), b"").unwrap();
    symlink("d", dir.join("l")).unwrap();
    let held = entered(&dir).unwrap();
    let entered = held.open_dir(&name("d")).unwrap();
    assert_eq!(entered.real().as_path(), dir.join("d"));
    assert_eq!(
        ObjectId::of(&entered.node.held.file).unwrap(),
        id(&dir.join("d"))
    );
    for refused in ["f", "l"] {
        let Err(err) = held.open_dir(&name(refused)) else {
            panic!("{refused} is no directory entry");
        };
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{refused}");
    }
    let Err(err) = held.open_dir(&name("missing")) else {
        panic!("nothing is there");
    };
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// The listing names the entries in order, and stats each as it is reached:
/// one removed after the first is yielded is seen to be gone.
#[test]
fn a_listing_is_sorted_and_stated_as_it_is_yielded() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    for entry in ["c", "a", "b"] {
        std::fs::write(dir.join(entry), entry).unwrap();
    }
    std::fs::create_dir(dir.join("d")).unwrap();
    symlink("a", dir.join("l")).unwrap();
    let held = entered(&dir).unwrap();

    let listed: Vec<(String, Kind)> = held
        .read_dir()
        .unwrap()
        .map(|e| {
            (
                e.name().as_os_str().to_str().unwrap().to_owned(),
                e.seen().unwrap().stat.kind,
            )
        })
        .collect();
    let want = [
        ("a", Kind::File),
        ("b", Kind::File),
        ("c", Kind::File),
        ("d", Kind::Dir),
        ("l", Kind::Symlink),
    ];
    assert_eq!(
        listed,
        want.map(|(n, k)| (n.to_owned(), k)).to_vec(),
        "a link is stated, not followed"
    );

    let mut entries = held.read_dir().unwrap();
    let first = entries.next().unwrap();
    assert_eq!(first.name().as_os_str(), "a");
    std::fs::remove_file(dir.join("b")).unwrap();
    let second = entries.next().unwrap();
    assert_eq!(second.name().as_os_str(), "b");
    assert_eq!(second.seen().err().unwrap().kind(), io::ErrorKind::NotFound);
    assert!(second.listed().is_none(), "an unseen entry has no identity");
    assert_eq!(entries.next().unwrap().name().as_os_str(), "c");
}

/// What the listing saw of an entry is its identity and kind, judged as the
/// name shown under the directory it was listed from.
#[test]
fn a_listed_entry_records_its_identity_beneath_its_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"").unwrap();
    let held = entered(&dir).unwrap();
    let entries: Vec<_> = held.read_dir().unwrap().collect();
    let [entry] = entries.as_slice() else {
        panic!("one entry");
    };
    let listed = entry.listed().unwrap();
    assert_eq!(listed.real().as_path(), dir.join("f"));
    assert_eq!(listed.step.id(), id(&dir.join("f")));
    assert_eq!(listed.step.kind(), Kind::File);
}

/// A directory the user may search and not list is walked, and refuses where
/// it is listed: at the reopen.
#[test]
fn listing_a_search_only_directory_fails_at_the_reopen_not_at_the_walk() {
    if root_user() {
        eprintln!("skip: root lists every directory");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("gate")).unwrap();
    std::fs::set_permissions(dir.join("gate"), PermissionsExt::from_mode(0o111)).unwrap();
    let held = entered(&dir.join("gate"));
    let listed = held.as_ref().map(|d| d.read_dir().err().map(|e| e.kind()));
    std::fs::set_permissions(dir.join("gate"), PermissionsExt::from_mode(0o755)).unwrap();
    assert_eq!(
        listed.unwrap(),
        Some(io::ErrorKind::PermissionDenied),
        "the walk entered it, the listing refused"
    );
}

/// A write staged beside a leaf lands in the directory the walk held, whatever
/// that directory is called by the time it is published; what is read back is
/// the installed object, not the staging handle.
#[test]
fn a_staged_write_publishes_under_the_held_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/f"), b"old").unwrap();
    let mut staged = located(&dir.join("d/f"))
        .unwrap()
        .stage(Visibility::Public)
        .unwrap();
    assert_eq!(staged.target().real().as_path(), dir.join("d/f"));
    staged.file().write_all(b"new").unwrap();
    staged.file().sync_all().unwrap();

    std::fs::rename(dir.join("d"), dir.join("moved")).unwrap();
    staged.publish().unwrap();

    assert_eq!(std::fs::read(dir.join("moved/f")).unwrap(), b"new");
    assert!(!dir.join("d").exists(), "the old name was not recreated");
    let beside: Vec<_> = std::fs::read_dir(dir.join("moved")).unwrap().collect();
    assert_eq!(beside.len(), 1, "no sibling is left beside the leaf");
}

#[test]
fn a_staged_write_over_an_absent_leaf_creates_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let mut staged = located(&dir.join("fresh"))
        .unwrap()
        .stage(Visibility::Public)
        .unwrap();
    staged.file().write_all(b"made").unwrap();
    assert!(!dir.join("fresh").exists());
    staged.publish().unwrap();
    assert_eq!(std::fs::read(dir.join("fresh")).unwrap(), b"made");
}

/// The staged file carries the leaf's mode, as the leaf had it at the walk.
#[test]
fn a_staged_write_carries_the_leafs_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"").unwrap();
    std::fs::set_permissions(dir.join("f"), PermissionsExt::from_mode(0o640)).unwrap();
    let mut staged = located(&dir.join("f"))
        .unwrap()
        .stage(Visibility::Public)
        .unwrap();
    let mode = |file: &File| file.metadata().unwrap().mode() & 0o7777;
    assert_eq!(mode(staged.file()), 0o640);
    staged.publish().unwrap();
    assert_eq!(
        std::fs::metadata(dir.join("f")).unwrap().mode() & 0o7777,
        0o640
    );
}

/// A file the user may not write is not staged over, though its directory
/// would let the rename replace it: nothing is made beside it.
#[test]
fn a_file_the_user_may_not_write_is_not_staged_over() {
    if root_user() {
        eprintln!("skip: root writes every file");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"old").unwrap();
    std::fs::set_permissions(dir.join("f"), PermissionsExt::from_mode(0o444)).unwrap();
    let err = located(&dir.join("f"))
        .unwrap()
        .stage(Visibility::Public)
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"old");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "nothing staged"
    );
}

/// The residual, stated as such: a rename selects its source by name, so a
/// sibling swapped at its name between the sync and the rename is what lands.
#[cfg(target_os = "linux")]
#[test]
fn a_sibling_swapped_at_its_name_before_the_rename_is_what_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"old").unwrap();
    let mut staged = located(&dir.join("f"))
        .unwrap()
        .stage(Visibility::Public)
        .unwrap();
    staged.file().write_all(b"staged").unwrap();
    staged.file().sync_all().unwrap();

    let sibling = dir.join(staged.name.as_os_str());
    std::fs::remove_file(&sibling).unwrap();
    std::fs::write(&sibling, b"substitute").unwrap();
    staged.publish().unwrap();
    assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"substitute");
}

#[test]
fn a_dropped_staged_write_leaves_no_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"old").unwrap();
    let staged = located(&dir.join("f"))
        .unwrap()
        .stage(Visibility::Public)
        .unwrap();
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        2,
        "staging makes one"
    );
    drop(staged);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"old");
}

#[test]
fn a_failed_publish_leaves_the_target_and_no_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"old").unwrap();
    let staged = located(&dir.join("f"))
        .unwrap()
        .stage(Visibility::Public)
        .unwrap();
    std::fs::remove_file(dir.join("f")).unwrap();
    std::fs::create_dir_all(dir.join("f/inside")).unwrap();
    assert!(
        staged.publish().is_err(),
        "a file cannot replace a directory"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    assert!(dir.join("f/inside").is_dir());
}

#[test]
fn a_directory_leaf_is_not_staged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    let err = located(&dir.join("d"))
        .unwrap()
        .stage(Visibility::Public)
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::IsADirectory);
}

/// A leaf replaced by rename between the walk and the open is read through
/// the handle the walk held, not through the name it had.
#[cfg(target_os = "linux")]
#[test]
fn a_leaf_replaced_after_the_walk_is_read_through_the_held_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"walked").unwrap();
    let held = located(&dir.join("f")).unwrap();
    std::fs::rename(dir.join("f"), dir.join("elsewhere")).unwrap();
    std::fs::write(dir.join("f"), b"replacement").unwrap();
    let mut body = String::new();
    held.read().unwrap().read_to_string(&mut body).unwrap();
    assert_eq!(body, "walked");
}

/// Reads and writes go through the leaf the walk held, and a leaf absent at
/// the walk is created exclusively.
#[test]
fn a_leaf_absent_at_the_walk_is_created_exclusively() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let target = located(&dir.join("fresh")).unwrap();
    std::fs::write(dir.join("fresh"), b"raced").unwrap();
    let err = target.open_streaming(Streaming::Append).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(dir.join("fresh")).unwrap(), b"raced");

    let target = located(&dir.join("made")).unwrap();
    let mut streamed = target.open_streaming(Streaming::Truncate).unwrap();
    assert!(streamed.created());
    streamed.file().write_all(b"body").unwrap();
    let reader = streamed.reader().unwrap();
    let mut body = String::new();
    reader.read_to_string(&mut body).unwrap();
    assert_eq!(body, "body");
    assert_eq!(streamed.trace().real().as_path(), dir.join("made"));
}

#[test]
fn a_streamed_truncate_runs_after_the_before_image_was_read() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"before").unwrap();
    let mut streamed = located(&dir.join("f"))
        .unwrap()
        .open_streaming(Streaming::Truncate)
        .unwrap();
    assert!(!streamed.created());
    let mut before = String::new();
    streamed
        .reader()
        .unwrap()
        .read_to_string(&mut before)
        .unwrap();
    assert_eq!(before, "before");
    assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"before");
    streamed.truncate().unwrap();
    assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"");
}

/// A FIFO has no inode to truncate and no image to read: it streams.
#[test]
fn a_streamed_truncate_on_a_fifo_is_a_no_op_that_streams() {
    use rustix::fs::OFlags;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let made = std::process::Command::new("mkfifo")
        .arg(dir.join("pipe"))
        .status()
        .unwrap();
    assert!(made.success());
    // A reader first: a write open on a FIFO with nobody to meet is refused.
    let mut sink = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NONBLOCK.bits().cast_signed())
        .open(dir.join("pipe"))
        .unwrap();
    let mut streamed = located(&dir.join("pipe"))
        .unwrap()
        .open_streaming(Streaming::Truncate)
        .unwrap();
    assert!(streamed.reader().is_none());
    streamed.truncate().unwrap();
    streamed.file().write_all(b"through").unwrap();
    let mut body = [0; 7];
    sink.read_exact(&mut body).unwrap();
    assert_eq!(&body, b"through");
}

#[test]
fn a_directory_leaf_is_not_read_or_written() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    symlink("d", dir.join("l")).unwrap();
    let held = located(&dir.join("d")).unwrap();
    assert_eq!(held.read().unwrap_err().kind(), io::ErrorKind::IsADirectory);
    assert_eq!(
        held.open_streaming(Streaming::Append).err().unwrap().kind(),
        io::ErrorKind::IsADirectory
    );
    let link = located_with(&dir.join("l"), LeafMode::NoFollow).unwrap();
    assert_eq!(link.read().unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

/// The directory a walk holds is the one `absent` and `create_dir` act in,
/// whatever it is called by then.
#[test]
fn an_absent_entry_is_made_in_the_directory_held() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    let held = entered(&dir.join("d")).unwrap();
    std::fs::rename(dir.join("d"), dir.join("moved")).unwrap();

    let target = writing(held.absent(name("sub")));
    assert_eq!(target.trace().tail(), [name("sub")]);
    assert!(target.stat().is_none());
    let made = target.create_dir(Visibility::Public).unwrap();
    assert!(dir.join("moved/sub").is_dir());
    assert!(!dir.join("d").exists());
    assert_eq!(made.real().as_path(), dir.join("d/sub"));
    assert_eq!(made.trace().tail(), []);

    let file = writing(held.absent(name("f")));
    file.open_streaming(Streaming::Append)
        .unwrap()
        .file()
        .write_all(b"x")
        .unwrap();
    assert_eq!(std::fs::read(dir.join("moved/f")).unwrap(), b"x");

    let Err(err) = located(&dir.join("moved/sub"))
        .unwrap()
        .create_dir(Visibility::Public)
    else {
        panic!("a present leaf is not made");
    };
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
}

/// What a vet is offered: the root, each directory on entry, each link before
/// it is spliced, and the leaf.
#[test]
fn a_vet_is_offered_every_object_the_walk_relies_on() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/f"), b"").unwrap();
    symlink("d", dir.join("l")).unwrap();
    let mut seen: Vec<(String, Option<Kind>)> = Vec::new();
    let located = walk_with(&spelled(&dir.join("l/f")), LeafMode::Follow, |v| {
        seen.push((v.name.to_str().unwrap().to_owned(), v.step.map(Step::kind)));
        assert_eq!(
            ObjectId::of(v.child.unwrap()).unwrap(),
            host::seen_at(v.dir, v.name).unwrap().id
        );
        true
    })
    .unwrap();
    assert_eq!(located.real().as_path(), dir.join("d/f"));
    let tail: Vec<_> = seen.iter().rev().take(3).rev().cloned().collect();
    assert_eq!(
        tail,
        [
            ("l".to_owned(), Some(Kind::Symlink)),
            ("d".to_owned(), Some(Kind::Dir)),
            ("f".to_owned(), Some(Kind::File)),
        ]
    );
    assert_eq!(seen[0], (".".to_owned(), None), "the root is offered first");
}

#[test]
fn a_vet_refusal_refuses_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"").unwrap();
    let refused = |name: &'static str| {
        walk_with(&spelled(&dir.join("f")), LeafMode::Follow, move |v| {
            v.name != name
        })
    };
    assert!(refused("elsewhere").is_ok());
    for name in [".", "f"] {
        let Err(err) = refused(name) else {
            panic!("the vet refused {name}");
        };
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{name}");
    }
}

/// What `exists`, `is-dir` and `shape` ask of the walk: a final link is seen
/// through by `Follow` and not by `NoFollow`; a dangling link does not exist
/// and is a link; the root answers like any spelling.
#[test]
fn the_probes_see_through_a_final_link_only_when_they_follow() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("f"), b"").unwrap();
    symlink("d", dir.join("to_d")).unwrap();
    symlink("f", dir.join("to_f")).unwrap();
    symlink("gone", dir.join("dangling")).unwrap();
    let kind = |rel: &str, mode| {
        located_with(&dir.join(rel), mode)
            .unwrap()
            .stat()
            .map(|s| s.kind)
    };
    let follow = |rel: &str| kind(rel, LeafMode::Follow);
    let named = |rel: &str| kind(rel, LeafMode::NoFollow);
    assert_eq!(follow("to_d"), Some(Kind::Dir));
    assert_eq!(follow("to_f"), Some(Kind::File));
    assert_eq!(follow("dangling"), None);
    assert_eq!(named("to_d"), Some(Kind::Symlink));
    assert_eq!(named("dangling"), Some(Kind::Symlink));
    assert_eq!(named("d"), Some(Kind::Dir));
    assert_eq!(named("f"), Some(Kind::File));
    assert_eq!(named("missing"), None);
    for mode in [LeafMode::Follow, LeafMode::NoFollow] {
        assert_eq!(
            located_with(Path::new("/"), mode)
                .unwrap()
                .stat()
                .unwrap()
                .kind,
            Kind::Dir
        );
    }
}

/// What `visibility` asks of a file made or staged: `Secret` and `Private` are
/// owner-only whatever was there, and `Public` keeps the mode of the leaf it
/// replaces.  A staged write over a 0644 leaf shows all three.
#[test]
fn a_staged_write_is_owner_only_unless_it_is_public() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    for (visibility, mode) in [
        (Visibility::Public, 0o644),
        (Visibility::Private, 0o600),
        (Visibility::Secret, 0o600),
    ] {
        let at = dir.join(format!("{visibility:?}"));
        std::fs::write(&at, b"old").unwrap();
        std::fs::set_permissions(&at, PermissionsExt::from_mode(0o644)).unwrap();
        let mut staged = located(&at).unwrap().stage(visibility).unwrap();
        staged.file().write_all(b"new").unwrap();
        staged.publish().unwrap();
        assert_eq!(std::fs::read(&at).unwrap(), b"new", "{visibility:?}");
        assert_eq!(
            std::fs::metadata(&at).unwrap().mode() & 0o7777,
            mode,
            "{visibility:?}"
        );
    }
}

/// A data open makes an absent leaf with the visibility it is given, and
/// touches a present one in no way: no truncation, and no change of mode.
#[test]
fn a_data_open_makes_an_absent_leaf_as_asked_and_leaves_a_present_one_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    for visibility in [Visibility::Private, Visibility::Secret] {
        let at = dir.join(format!("{visibility:?}"));
        let mut file = located(&at)
            .unwrap()
            .open_data(Access::Write, visibility)
            .unwrap();
        file.write_all(b"made").unwrap();
        assert_eq!(
            std::fs::metadata(&at).unwrap().mode() & 0o7777,
            0o600,
            "{visibility:?}"
        );
    }

    let at = dir.join("present");
    std::fs::write(&at, b"body").unwrap();
    std::fs::set_permissions(&at, PermissionsExt::from_mode(0o644)).unwrap();
    for access in [Access::Read, Access::Append, Access::Write] {
        located(&at)
            .unwrap()
            .open_data(access, Visibility::Secret)
            .unwrap();
    }
    assert_eq!(std::fs::read(&at).unwrap(), b"body");
    assert_eq!(std::fs::metadata(&at).unwrap().mode() & 0o7777, 0o644);

    let raced = located(&dir.join("raced")).unwrap();
    std::fs::write(dir.join("raced"), b"first").unwrap();
    let err = raced
        .open_data(Access::Write, Visibility::Secret)
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(dir.join("raced")).unwrap(), b"first");
}

/// A file made by `create_file` is made as it is told, and never over a
/// present one.
#[test]
fn a_file_made_without_a_handle_is_made_as_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    located(&dir.join("secret"))
        .unwrap()
        .create_file(Visibility::Secret)
        .unwrap();
    assert_eq!(
        std::fs::metadata(dir.join("secret")).unwrap().mode() & 0o7777,
        0o600
    );
    let again = located(&dir.join("secret"))
        .unwrap()
        .create_file(Visibility::Secret)
        .unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
}

/// `create_new` hands back the handle of the file it makes, and answers
/// `AlreadyExists` for any present leaf, a link included, without making
/// anything.
#[test]
fn create_new_refuses_a_present_leaf_and_hands_back_the_handle_of_an_absent_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    let target = |name: &str| located_with(&dir.join(name), LeafMode::NoFollow).unwrap();

    let mut made = target("new")
        .create_new(Access::Write, Visibility::Public)
        .unwrap();
    made.write_all(b"made").unwrap();
    assert_eq!(std::fs::read(dir.join("new")).unwrap(), b"made");

    let present = target("new")
        .create_new(Access::Write, Visibility::Public)
        .unwrap_err();
    assert_eq!(present.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(dir.join("new")).unwrap(), b"made");

    symlink(dir.join("nowhere"), dir.join("l")).unwrap();
    let linked = target("l")
        .create_new(Access::Write, Visibility::Public)
        .unwrap_err();
    assert_eq!(linked.kind(), io::ErrorKind::AlreadyExists);
    assert!(
        std::fs::symlink_metadata(dir.join("nowhere")).is_err(),
        "nothing was made through the link"
    );
}

/// The making walk makes each absent name of the spelling owner-only, steps
/// onto it, and takes a present name as it is.
#[test]
fn the_making_walk_makes_each_absent_name_owner_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("present")).unwrap();
    std::fs::set_permissions(dir.join("present"), PermissionsExt::from_mode(0o755)).unwrap();

    let made = walk_dir_made(&spelled(&dir.join("present/a/b"))).unwrap();
    assert_eq!(made.real().as_path(), dir.join("present/a/b"));
    let mode = |rel: &str| std::fs::metadata(dir.join(rel)).unwrap().mode() & 0o7777;
    assert_eq!(mode("present"), 0o755);
    assert_eq!(mode("present/a"), 0o700);
    assert_eq!(mode("present/a/b"), 0o700);

    let again = walk_dir_made(&spelled(&dir.join("present/a/b"))).unwrap();
    assert_eq!(again.trace().last_id(), made.trace().last_id());

    std::fs::write(dir.join("file"), b"").unwrap();
    for under in ["file", "file/sub"] {
        let err = walk_dir_made(&spelled(&dir.join(under)))
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{under}");
    }
}

/// A link on the way is followed, and what the spelling names past it is made
/// where the link leads.
#[test]
fn the_making_walk_makes_the_spelled_names_where_a_link_leads() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("real")).unwrap();
    symlink("real", dir.join("alias")).unwrap();

    let made = walk_dir_made(&spelled(&dir.join("alias/new/deeper"))).unwrap();
    assert_eq!(made.real().as_path(), dir.join("real/new/deeper"));
    assert!(dir.join("real/new/deeper").is_dir());

    let linked = walk_dir_made(&spelled(&dir.join("alias"))).unwrap();
    assert_eq!(linked.real().as_path(), dir.join("real"));
}

/// A name a link's target spliced in is never made: a dangling link fails the
/// walk, as it fails `mkdir -p`, and nothing is made through it.
#[test]
fn the_making_walk_refuses_a_dangling_link() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("real")).unwrap();
    symlink("gone/inside", dir.join("far")).unwrap();
    symlink("real/missing", dir.join("near")).unwrap();
    symlink("nowhere", dir.join("middle")).unwrap();

    for (spelling, target) in [
        ("far", Some("gone/inside")),
        ("near", Some("real/missing")),
        ("middle/below", None),
    ] {
        let err = walk_dir_made(&spelled(&dir.join(spelling)))
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{spelling}");
        assert_eq!(dangling(&err), target.map(Path::new), "{spelling}");
    }
    for made in ["gone", "real/missing", "nowhere"] {
        assert!(!dir.join(made).exists(), "{made} was made through a link");
    }
}

/// `target` is `locate` that also answers a create target, and through a
/// dangling link it is the target's, with the link's target said.
#[test]
fn target_yields_a_create_target_through_a_dangling_link() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("real")).unwrap();
    std::fs::write(dir.join("real/present"), b"body").unwrap();
    symlink("real/made", dir.join("dangling")).unwrap();
    symlink("real/present", dir.join("live")).unwrap();
    let held = entered(&dir).unwrap();
    let target = |entry: &str, mode| held.target(&name(entry), mode).map(writing);

    let through = target("dangling", LeafMode::Follow).unwrap();
    assert!(through.stat().is_none());
    assert_eq!(through.real().as_path(), dir.join("real/made"));
    assert_eq!(through.linked_to(), Some(Path::new("real/made")));
    through
        .open_data(Access::Write, Visibility::Secret)
        .unwrap()
        .write_all(b"made")
        .unwrap();
    assert_eq!(std::fs::read(dir.join("dangling")).unwrap(), b"made");
    assert_eq!(
        std::fs::metadata(dir.join("real/made")).unwrap().mode() & 0o7777,
        0o600
    );
    let located = held
        .locate(&name("dangling"), LeafMode::Follow)
        .map(writing)
        .unwrap();
    assert_eq!(located.stat().unwrap().kind, Kind::File);

    std::fs::remove_file(dir.join("real/made")).unwrap();
    let refused = held
        .locate(&name("dangling"), LeafMode::Follow)
        .err()
        .unwrap();
    assert_eq!(refused.kind(), io::ErrorKind::NotFound);

    let live = target("live", LeafMode::Follow).unwrap();
    assert_eq!(live.stat().unwrap().kind, Kind::File);
    assert_eq!(live.linked_to(), Some(Path::new("real/present")));
    let named = target("live", LeafMode::NoFollow).unwrap();
    assert_eq!(named.stat().unwrap().kind, Kind::Symlink);
    assert_eq!(named.linked_to(), None, "a link not followed led nowhere");
    let plain = target("real", LeafMode::Follow).unwrap();
    assert_eq!(plain.linked_to(), None);
    let absent = target("nothing", LeafMode::Follow).unwrap();
    assert_eq!(absent.real().as_path(), dir.join("nothing"));
    assert_eq!(absent.linked_to(), None);
}

/// A link into a folder that is not there is a `NotFound` that says where the
/// link led, whoever walked it; any other error, and a `NotFound` met before
/// a link at the last name, says nothing of one.
#[test]
fn dangling_reads_a_missing_target_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    symlink("missing/folder/f", dir.join("l")).unwrap();
    let held = entered(&dir).unwrap();

    let by_target = held.target(&name("l"), LeafMode::Follow).err().unwrap();
    let by_walk = walk(&spelled(&dir.join("l")), LeafMode::Follow)
        .map(|_| ())
        .unwrap_err();
    for err in [by_target, by_walk] {
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(dangling(&err), Some(Path::new("missing/folder/f")));
        assert_eq!(
            err.to_string(),
            io::Error::from(rustix::io::Errno::NOENT).to_string(),
            "the walk's own message"
        );
    }

    let plain = walk(&spelled(&dir.join("missing/f")), LeafMode::Follow)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(plain.kind(), io::ErrorKind::NotFound);
    assert_eq!(dangling(&plain), None);
    assert_eq!(dangling(&loop_error()), None);
    assert_eq!(dangling(&io::Error::from(io::ErrorKind::NotFound)), None);
}

/// `remove` takes a tree with it, a link as the link: what a link leads to is
/// never entered or touched.
#[test]
fn remove_unlinks_links_in_a_tree_and_never_follows_them() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir_all(dir.join("keep/inner")).unwrap();
    std::fs::write(dir.join("keep/file"), b"kept").unwrap();
    std::fs::write(dir.join("keep/inner/deep"), b"kept").unwrap();
    std::fs::create_dir_all(dir.join("tree/sub/deeper")).unwrap();
    std::fs::write(dir.join("tree/f"), b"").unwrap();
    std::fs::write(dir.join("tree/sub/g"), b"").unwrap();
    std::fs::write(dir.join("tree/sub/deeper/h"), b"").unwrap();
    symlink("../keep", dir.join("tree/to_dir")).unwrap();
    symlink("../../keep/file", dir.join("tree/sub/to_file")).unwrap();
    symlink(dir.join("keep"), dir.join("tree/sub/deeper/absolute")).unwrap();
    symlink("nowhere", dir.join("tree/dangling")).unwrap();
    symlink("../tree", dir.join("tree/sub/loop")).unwrap();

    located_with(&dir.join("tree"), LeafMode::NoFollow)
        .unwrap()
        .remove()
        .unwrap();

    assert!(!dir.join("tree").exists());
    assert_eq!(std::fs::read(dir.join("keep/file")).unwrap(), b"kept");
    assert_eq!(std::fs::read(dir.join("keep/inner/deep")).unwrap(), b"kept");
    assert_eq!(std::fs::read_dir(dir.join("keep")).unwrap().count(), 2);
}

/// A file goes, a link goes as the link whatever it leads to, and what is not
/// there, or is no directory's entry, is refused.
#[test]
fn remove_takes_the_entry_and_refuses_what_is_not_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"body").unwrap();
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/inside"), b"body").unwrap();
    symlink("d", dir.join("to_d")).unwrap();

    located(&dir.join("f")).unwrap().remove().unwrap();
    assert!(!dir.join("f").exists());

    located_with(&dir.join("to_d"), LeafMode::NoFollow)
        .unwrap()
        .remove()
        .unwrap();
    assert!(
        dir.join("d/inside").exists(),
        "the link's target is untouched"
    );
    assert!(std::fs::symlink_metadata(dir.join("to_d")).is_err());

    let absent = located(&dir.join("absent")).unwrap();
    assert_eq!(absent.remove().unwrap_err().kind(), io::ErrorKind::NotFound);
    let root = located_with(Path::new("/"), LeafMode::NoFollow).unwrap();
    assert_eq!(
        root.present().map(|_| ()).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

/// A removal is by the directory handle held: a directory that moved is
/// still the one removed from.
#[test]
fn remove_acts_in_the_directory_held() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/f"), b"").unwrap();
    let held = located(&dir.join("d/f")).unwrap();
    std::fs::rename(dir.join("d"), dir.join("moved")).unwrap();
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/f"), b"decoy").unwrap();

    held.remove().unwrap();
    assert!(!dir.join("moved/f").exists());
    assert_eq!(std::fs::read(dir.join("d/f")).unwrap(), b"decoy");
}

/// A rename lands over what is at the new name, within the directory held,
/// and takes a link as the link.
#[test]
fn rename_replaces() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("f"), b"new").unwrap();
    std::fs::write(dir.join("g"), b"old").unwrap();
    std::fs::write(dir.join("elsewhere"), b"untouched").unwrap();
    symlink("elsewhere", dir.join("l")).unwrap();

    located(&dir.join("f")).unwrap().rename(&name("g")).unwrap();
    assert!(!dir.join("f").exists());
    assert_eq!(std::fs::read(dir.join("g")).unwrap(), b"new");

    located(&dir.join("g"))
        .unwrap()
        .rename(&name("fresh"))
        .unwrap();
    assert_eq!(std::fs::read(dir.join("fresh")).unwrap(), b"new");

    located_with(&dir.join("l"), LeafMode::NoFollow)
        .unwrap()
        .rename(&name("fresh"))
        .unwrap();
    assert_eq!(
        std::fs::read_link(dir.join("fresh")).unwrap(),
        Path::new("elsewhere")
    );
    assert_eq!(std::fs::read(dir.join("elsewhere")).unwrap(), b"untouched");

    std::fs::create_dir(dir.join("d")).unwrap();
    located(&dir.join("d")).unwrap().rename(&name("e")).unwrap();
    assert!(dir.join("e").is_dir() && !dir.join("d").exists());

    let absent = located(&dir.join("absent")).unwrap();
    assert_eq!(
        absent.rename(&name("x")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

/// A rename is within the directory held, whatever it is called by then.
#[test]
fn rename_acts_in_the_directory_held() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("d/f"), b"held").unwrap();
    let held = located(&dir.join("d/f")).unwrap();
    std::fs::rename(dir.join("d"), dir.join("moved")).unwrap();

    held.rename(&name("g")).unwrap();
    assert_eq!(std::fs::read(dir.join("moved/g")).unwrap(), b"held");
    assert!(!dir.join("d").exists());
}

/// `exists`, `is_dir` and `kind_of` are one walk, followed to the end.
#[test]
fn kind_of_is_what_the_absolute_spelling_reaches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::create_dir(dir.join("d")).unwrap();
    std::fs::write(dir.join("f"), b"").unwrap();
    symlink("d", dir.join("to_d")).unwrap();
    symlink("gone", dir.join("dangling")).unwrap();
    let at = |rel: &str| shown(dir.join(rel));

    assert_eq!(kind_of(&at("d")), Some(Kind::Dir));
    assert_eq!(kind_of(&at("f")), Some(Kind::File));
    assert_eq!(kind_of(&at("to_d")), Some(Kind::Dir));
    assert_eq!(kind_of(&at("dangling")), None);
    assert_eq!(kind_of(&at("absent")), None);
    assert_eq!(kind_of("relative/path"), None);
    assert!(exists(&at("f")) && !exists(&at("dangling")));
    assert!(is_dir(&at("to_d")) && !is_dir(&at("f")));
}
