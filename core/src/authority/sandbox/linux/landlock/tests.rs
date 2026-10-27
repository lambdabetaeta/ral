use super::*;
use crate::authority::policy::{ExecAllow, ExecDeny, Extent, GrantPath, Rule};
use crate::authority::sandbox::{FsProjection, FsRules};
use crate::path::{Excluded, FrozenPath, render_objects, utf8};
use crate::test_helper::confinement::skip;
use crate::test_helper::{Tree, place, real};
use rustix::fs::FileType;
use std::path::{Path, PathBuf};

/// The roots of an unrestricted fs.
fn whole() -> Vec<RealPath> {
    vec![real("/")]
}

/// The position of the `/`-rooted fixture name `spelled`, under a place
/// nothing occupies.
fn nowhere(spelled: &str) -> RealPath {
    place(Tree::nowhere().path(spelled))
}

fn restricted() -> ExecProjection {
    ExecProjection::Restricted(ExecRules::default())
}

fn table_of(rules: &[Rule]) -> ExecRules {
    rules.iter().cloned().collect()
}

#[test]
fn only_the_two_absence_errnos_read_as_a_kernel_without_landlock() {
    assert_eq!(classify(5, 0), Landlock::At(Abi(5)));
    assert_eq!(classify(-1, libc::ENOSYS), Landlock::Absent);
    assert_eq!(classify(-1, libc::EOPNOTSUPP), Landlock::Absent);
    assert_eq!(
        classify(-1, libc::EPERM),
        Landlock::Unprobed(libc::EPERM),
        "a seccomp EPERM is not absence"
    );
}

/// Landlock is the only kernel exec layer on Linux, so a grant that limits
/// which programs run is refused where there is none, not run unchecked;
/// a failed probe tells nothing, so it refuses every launch.
#[test]
fn an_exec_restricting_grant_is_refused_without_landlock() {
    let why = plan(&restricted(), Landlock::Absent)
        .expect_err("a restricted exec grant needs Landlock")
        .to_string();
    assert!(
        why.starts_with("sandbox confinement unavailable: "),
        "{why}"
    );
    assert!(why.contains("Landlock") && why.contains("5.13"), "{why}");
    assert_eq!(
        plan(&ExecProjection::Unrestricted, Landlock::Absent).expect("nothing to enforce"),
        None
    );
    for exec in [restricted(), ExecProjection::Unrestricted] {
        let why = plan(&exec, Landlock::Unprobed(libc::EPERM))
            .expect_err("a failed probe is not a kernel without Landlock")
            .to_string();
        assert!(
            why.starts_with("sandbox confinement unavailable: "),
            "{why}"
        );
        assert!(why.contains("probe failed"), "{why}");
    }
}

#[test]
fn a_restricted_projection_handles_refer_from_abi_two_and_scopes_from_six() {
    let exec = Access::EXECUTE;
    let refer = Access::EXECUTE | Access::REFER;
    for (abi, handled, scoped) in [
        (Abi::EXEC, exec, Scoped::NONE),
        (Abi::REFER, refer, Scoped::NONE),
        (Abi(5), refer, Scoped::NONE),
        (Abi::SCOPES, refer, Scoped::ENVELOPE),
        (Abi(7), refer, Scoped::ENVELOPE),
        (Abi(8), refer, Scoped::ENVELOPE),
        (
            Abi::RESOLVE_UNIX,
            refer | Access::RESOLVE_UNIX,
            Scoped::ENVELOPE,
        ),
        (Abi(10), refer | Access::RESOLVE_UNIX, Scoped::ENVELOPE),
    ] {
        assert_eq!(
            plan(&restricted(), Landlock::At(abi)).expect("a kernel with Landlock"),
            Some(Plan { handled, scoped }),
            "abi {abi}"
        );
    }
}

#[test]
fn an_unrestricted_exec_projection_handles_sockets_from_abi_nine() {
    for abi in [Abi::EXEC, Abi::REFER, Abi(5)] {
        assert_eq!(
            plan(&ExecProjection::Unrestricted, Landlock::At(abi)).expect("plans"),
            None,
            "abi {abi} has no scopes, so there is nothing to enter"
        );
    }
    for (abi, handled) in [
        (Abi::SCOPES, Access::NONE),
        (Abi(7), Access::NONE),
        (Abi(8), Access::NONE),
        (Abi::RESOLVE_UNIX, Access::REFER | Access::RESOLVE_UNIX),
        (Abi(10), Access::REFER | Access::RESOLVE_UNIX),
    ] {
        assert_eq!(
            plan(&ExecProjection::Unrestricted, Landlock::At(abi)).expect("plans"),
            Some(Plan {
                handled,
                scoped: Scoped::ENVELOPE
            }),
            "abi {abi}"
        );
    }
}

/// The live kernel, where the build itself is under test: without one
/// there is no ruleset to create, so the test stops, or fails where the
/// sandbox is required.
fn landlock() -> Option<Landlock> {
    let probed = Landlock::probe();
    matches!(probed, Landlock::At(_))
        .then_some(probed)
        .or_else(|| {
            skip(probed);
            None
        })
}

fn rule(real: RealPath, dir: bool, allow: bool) -> Rule {
    let written = FrozenPath::from_surface(utf8(real.as_path()).unwrap());
    match (dir, allow) {
        (true, true) => Rule::Allow(ExecAllow::Dir(GrantPath {
            written,
            real: Extent::Under {
                position: real,
                passed: Vec::new(),
            },
        })),
        (true, false) => Rule::Deny(Box::new(ExecDeny::Dir(
            GrantPath::<Excluded>::walk(written).expect("a directory walks"),
        ))),
        (false, true) => Rule::Allow(ExecAllow::support(&real)),
        (false, false) => Rule::Deny(Box::new(ExecDeny::File(
            GrantPath::<Excluded>::walk(written).expect("a file walks"),
        ))),
    }
}

fn allow(path: &Path, dir: bool) -> Rule {
    rule(place(path), dir, true)
}

fn deny(path: &Path, dir: bool) -> Rule {
    rule(place(path), dir, false)
}

fn veto(name: &str) -> Rule {
    Rule::Deny(Box::new(ExecDeny::Name(crate::path::command_name_key(
        name,
    ))))
}

fn any() -> Rule {
    Rule::Allow(ExecAllow::Any)
}

/// An admit naming nothing, and a file admit since replaced by a
/// directory, admit nothing; neither is a reason to refuse the launch.
#[test]
fn an_absent_admit_and_a_file_admit_now_a_directory_are_dropped() {
    let Some(landlock) = landlock() else {
        return;
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let rules = [
        allow(&tmp.path().join("absent"), true),
        allow(&tmp.path().join("absent/beneath"), false),
        allow(tmp.path(), false),
    ];
    let built = build(
        &ExecProjection::Restricted(table_of(&rules)),
        &FsProjection::Unrestricted,
        &whole(),
        landlock,
        &mut Handoff::default(),
    )
    .expect("builds");
    assert!(built.is_some(), "a restricted projection is entered");
    let files = admitted_files(&rules).expect("admits");
    assert!(files.is_empty(), "a file admit named {files:?}");
    let dirs = expanded(&rules, true).expect("expands");
    assert!(dirs.is_empty(), "a directory admit named {dirs:?}");
}

/// A grant renders real paths, so a symlink at one now was planted since.
#[test]
fn an_admit_now_reached_through_a_symlink_refuses_the_launch() {
    let Some(landlock) = landlock() else {
        return;
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let outside = tmp.path().join("outside");
    std::fs::create_dir(&outside).expect("outside");
    let allowed = tmp.path().join("allowed");
    let position = place(&allowed);
    std::os::unix::fs::symlink(&outside, &allowed).expect("symlink");
    let Err(why) = build(
        &ExecProjection::Restricted(table_of(&[rule(position, true, true)])),
        &FsProjection::Unrestricted,
        &whole(),
        landlock,
        &mut Handoff::default(),
    ) else {
        panic!("a symlink would admit its target");
    };
    assert!(why.to_string().contains("symbolic link"), "{why}");
}

// ── Expansion ────────────────────────────────────────────────────────

/// Every regular file of [`tree`].
const FILES: [&str; 8] = ["a", "x", "n", "s/f", "s/t/g", "s/t/n", "sub/h", "sub/n"];

/// [`FILES`] under a real `d`.
fn tree() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let d = real(tmp.path()).as_path().join("d");
    for file in FILES {
        let path = d.join(file);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
        std::fs::write(&path, "").expect("file");
    }
    (tmp, d)
}

/// What [`expand`] hands its sink, each handle checked against its name,
/// and whether it stands for a hierarchy.
fn expanded(rules: &[Rule], walks: bool) -> Result<BTreeSet<(PathBuf, bool)>, Error> {
    expanded_on(rules, &whole(), walks)
}

fn expanded_on(
    rules: &[Rule],
    roots: &[RealPath],
    walks: bool,
) -> Result<BTreeSet<(PathBuf, bool)>, Error> {
    expanded_masking(rules, roots, walks, &[])
}

fn expanded_masking(
    rules: &[Rule],
    roots: &[RealPath],
    walks: bool,
    masks: &[RealPath],
) -> Result<BTreeSet<(PathBuf, bool)>, Error> {
    use std::os::unix::fs::MetadataExt;
    let mut seen = BTreeSet::new();
    expand(&table_of(rules), roots, &|_| walks, masks, &mut |admit| {
        let path = admit.path.as_path();
        let opened = rustix::fs::fstat(admit.fd)?;
        assert_eq!(
            opened.st_ino,
            std::fs::symlink_metadata(path)?.ino(),
            "{}",
            admit.path
        );
        let whole = FileType::from_raw_mode(opened.st_mode) == FileType::Directory;
        seen.insert((path.to_path_buf(), whole));
        Ok(())
    })?;
    Ok(seen)
}

fn admits(d: &Path, admitted: &[(&str, bool)]) -> BTreeSet<(PathBuf, bool)> {
    (admitted.iter())
        .map(|&(rel, whole)| (d.join(rel), whole))
        .collect()
}

/// Each admit is the table's own verdict on a file the walk reached: a
/// denied file absent, a denied directory never entered and an allow
/// inside it no root (an exclusion is never reopened, §7.5), a vetoed
/// name absent wherever it lies.
#[test]
fn an_expansion_admits_exactly_what_the_table_allows() {
    let (_tmp, d) = tree();
    let rules = [
        allow(&d, true),
        deny(&d.join("x"), false),
        deny(&d.join("s"), true),
        allow(&d.join("s/t"), true),
        veto("n"),
    ];
    let table = table_of(&rules);
    let model: BTreeSet<_> = (FILES.iter())
        .map(|f| d.join(f))
        .filter(|f| {
            table
                .evidence(Subject::Program(crate::path::Subject::Position(&real(f))))
                .permits()
        })
        .map(|f| (f, false))
        .collect();
    assert_eq!(model, admits(&d, &[("a", false), ("sub/h", false)]));
    assert_eq!(expanded(&rules, true).expect("expands"), model);
}

/// Only the spine to a block is listed; every directory off it is one
/// rule, however much lies beneath.
#[test]
fn a_directory_with_no_block_beneath_it_is_one_whole_admit() {
    let (_tmp, d) = tree();
    assert_eq!(
        expanded(&[allow(&d, true)], true).expect("expands"),
        admits(&d, &[("", true)])
    );
    let spine = admits(
        &d,
        &[
            ("a", false),
            ("x", false),
            ("n", false),
            ("sub", true),
            ("s/t", true),
        ],
    );
    let rules = [allow(&d, true), deny(&d.join("s/f"), false)];
    assert_eq!(expanded(&rules, true).expect("expands"), spine);
}

/// A deny holds every spelling of its name, in the kernel as in the guard.
#[test]
fn a_deny_by_another_spelling_blocks_the_entry() {
    let (_tmp, d) = tree();
    let rules = [allow(&d, true), deny(&d.join("X"), false)];
    let seen = expanded(&rules, true).expect("expands");
    assert!(!seen.contains(&(d.join("x"), false)), "{seen:?}");
    assert!(seen.contains(&(d.join("a"), false)), "{seen:?}");
}

#[test]
fn a_listed_hard_link_to_an_excluded_file_is_not_admitted() {
    let (_tmp, d) = tree();
    std::fs::hard_link(d.join("x"), d.join("alias")).unwrap();
    let seen = expanded(&[allow(&d, true), deny(&d.join("x"), false)], true).unwrap();
    assert!(!seen.contains(&(d.join("alias"), false)), "{seen:?}");
    assert!(seen.contains(&(d.join("a"), false)), "{seen:?}");
}

#[test]
fn a_moved_exclusion_holding_directory_is_still_listed() {
    let (_tmp, d) = tree();
    let rules = [allow(&d, true), deny(&d.join("s/f"), false)];
    std::fs::rename(d.join("s"), d.join("moved")).unwrap();
    let seen = expanded(&rules, true).unwrap();
    assert!(!seen.contains(&(d.join("moved"), true)), "{seen:?}");
    assert!(!seen.contains(&(d.join("moved/f"), false)), "{seen:?}");
    assert!(seen.contains(&(d.join("moved/t"), true)), "{seen:?}");
}

#[test]
fn a_link_exclusion_keeps_its_targets_spine_and_identity() {
    let (_tmp, d) = tree();
    let link = d.join("link");
    std::os::unix::fs::symlink("s/f", &link).unwrap();
    let excluded = GrantPath::<Excluded>::walk(FrozenPath::freeze(&link).unwrap()).unwrap();
    let rules = [
        allow(&d, true),
        Rule::Deny(Box::new(ExecDeny::File(excluded))),
    ];
    std::fs::rename(d.join("s"), d.join("moved")).unwrap();
    let seen = expanded(&rules, true).unwrap();
    assert!(!seen.contains(&(d.join("moved"), true)), "{seen:?}");
    assert!(!seen.contains(&(d.join("moved/f"), false)), "{seen:?}");
    assert!(seen.contains(&(d.join("moved/t"), true)), "{seen:?}");
}

#[test]
fn a_recreated_excluded_directory_is_still_denied_by_name() {
    let (_tmp, d) = tree();
    let rules = [allow(&d, true), deny(&d.join("s"), true)];
    std::fs::rename(d.join("s"), d.join("moved")).unwrap();
    std::fs::create_dir(d.join("s")).unwrap();
    std::fs::write(d.join("s/new"), "").unwrap();
    let seen = expanded(&rules, true).unwrap();
    assert!(
        !seen.iter().any(|(p, _)| p.starts_with(d.join("s"))),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|(p, _)| p.starts_with(d.join("moved"))),
        "{seen:?}"
    );
    assert!(seen.contains(&(d.join("a"), false)), "{seen:?}");
}

#[test]
fn an_absent_exclusion_lists_its_last_ancestor_without_excluding_it() {
    let (_tmp, d) = tree();
    let rules = [allow(&d, true), deny(&d.join("s/absent/tool"), false)];
    std::fs::rename(d.join("s"), d.join("moved")).unwrap();
    let seen = expanded(&rules, true).unwrap();
    assert!(!seen.contains(&(d.join("moved"), true)), "{seen:?}");
    assert!(seen.contains(&(d.join("moved/f"), false)), "{seen:?}");
}

/// Where vetoes do not walk, a vetoed name costs no listing; where they
/// do, it is gone from every directory.
#[test]
fn a_veto_is_carried_only_where_vetoes_walk() {
    let (_tmp, d) = tree();
    let rules = [allow(&d, true), veto("n")];
    assert_eq!(
        expanded(&rules, false).expect("expands"),
        admits(&d, &[("", true)])
    );
    let seen = expanded(&rules, true).expect("expands");
    for vetoed in ["n", "sub/n", "s/t/n"] {
        assert!(
            !seen.contains(&(d.join(vetoed), false)),
            "{vetoed}: {seen:?}"
        );
    }
    assert!(seen.contains(&(d.join("sub/h"), false)), "{seen:?}");
}

/// The files `admit_files` admits for `rules`.
fn admitted_files(rules: &[Rule]) -> Result<BTreeSet<PathBuf>, Error> {
    let mut seen = BTreeSet::new();
    admit_files(&table_of(rules), &mut |admit| {
        seen.insert(admit.path.as_path().to_path_buf());
        Ok(())
    })?;
    Ok(seen)
}

/// An allowed executable replaced by a directory holding another
/// executable admits nothing beneath it: a directory rule would admit
/// its descendants, and a file admit names that file alone.
#[test]
fn an_allowed_executable_now_a_directory_admits_nothing_beneath() {
    let (_tmp, d) = tree();
    let seen = admitted_files(&[allow(&d.join("sub"), false)]).expect("admits");
    assert!(seen.is_empty(), "{seen:?}");
    let seen = admitted_files(&[allow(&d.join("a"), false)]).expect("admits");
    assert_eq!(seen, BTreeSet::from([d.join("a")]), "a file is admitted");
}

#[test]
fn an_absent_root_is_dropped() {
    let (_tmp, d) = tree();
    let seen = expanded(&[allow(&d.join("absent"), true)], true).expect("expands");
    assert!(seen.is_empty(), "{seen:?}");
}

/// A grant renders real paths, so a root that is a symlink now was
/// planted since, which refuses rather than admit its target.
#[test]
fn a_root_now_a_symlink_refuses_the_expansion() {
    let (_tmp, d) = tree();
    let link = d.join("link");
    let position = place(&link);
    std::os::unix::fs::symlink(d.join("sub"), &link).expect("symlink");
    let refused = expanded(&[rule(position, true, true)], true);
    assert!(
        matches!(refused, Err(Error::Open(OpenError::Linked(_)))),
        "{refused:?}"
    );
}

/// The write prefixes of an fs projection, each a rendered object.
fn writing(prefixes: &[&str]) -> RenderedFs {
    let prefixes: Vec<_> = prefixes.iter().map(|p| nowhere(p)).collect();
    let none: &[RealPath] = &[];
    let write = render_objects(prefixes.iter().map(|p| (p, None, none))).expect("renders");
    FsProjection::Restricted(FsRules {
        write,
        ..FsRules::default()
    })
}

/// A name veto walks a directory only where no write reaches it, whether
/// the write holds the directory or lies inside it, and never beneath
/// `Any` or under an unrestricted fs, where every write does.
#[test]
fn a_name_veto_walks_only_where_no_write_reaches() {
    let bin = nowhere("/ral-test/w/bin");
    let table = table_of(&[allow(bin.as_path(), true), veto("n")]);
    let walks = |fs: &RenderedFs| vetoes_walk(&table, fs)(&bin);
    assert!(walks(&writing(&[])), "apart");
    assert!(walks(&writing(&["/ral-test/other"])), "elsewhere");
    assert!(!walks(&writing(&["/ral-test/w"])), "within a write");
    assert!(!walks(&writing(&["/ral-test/w/bin"])), "the write itself");
    assert!(!walks(&writing(&["/ral-test/w/bin/out"])), "a write inside");
    assert!(!walks(&FsProjection::Unrestricted), "every write reaches");
    let beneath_any = table_of(&[any(), allow(bin.as_path(), true), veto("n")]);
    assert!(
        !vetoes_walk(&beneath_any, &writing(&[]))(&bin),
        "beneath Any"
    );
}

/// Every directory admission is lowered onto the bind roots: a rule on
/// the directory where a root lies at or above it, one on each root
/// inside it, `Any` being every root.
#[test]
fn a_directory_admission_is_lowered_onto_the_bind_roots() {
    let nowhere = Tree::nowhere();
    let lowered = |rules: &[Rule], roots: &[&str]| {
        let roots: Vec<_> = roots.iter().map(|r| place(nowhere.path(r))).collect();
        let table = table_of(rules);
        let dirs = table
            .allowed_dirs()
            .chain(table.supports(Subject::Any).then(RealPath::universal));
        lowered(dirs, &roots)
            .iter()
            .map(|r| r.as_path().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    let at = |names: &[&str]| names.iter().map(|n| nowhere.at(n)).collect::<Vec<_>>();
    let tools = [allow(&nowhere.path("/tools"), true)];
    assert_eq!(lowered(&tools, &["/"]), at(&["/tools"]), "a root above");
    assert_eq!(
        lowered(&tools, &["/tools"]),
        at(&["/tools"]),
        "a root at it"
    );
    assert_eq!(
        lowered(&tools, &["/tools/sub"]),
        at(&["/tools/sub"]),
        "a sub-bind"
    );
    assert_eq!(
        lowered(&tools, &["/", "/tools/sub", "/other"]),
        at(&["/tools", "/tools/sub"]),
        "above and inside, never beside"
    );
    assert!(lowered(&tools, &["/other"]).is_empty(), "not shown at all");
    assert_eq!(
        lowered(&[any()], &["/usr", "/work"]),
        at(&["/usr", "/work"]),
        "Any is every root"
    );
    assert_eq!(lowered(&[any()], &["/"]), at(&["/"]), "or `/` itself");
}

/// An `every` table whose exclusions are all name vetoes subtracts
/// nothing, so the kernel handles no exec for it and keeps its
/// scopes; one that excludes a path still does.
#[test]
fn an_every_table_with_only_name_vetoes_handles_no_exec() {
    let of = |rules: &[Rule]| ExecProjection::Restricted(table_of(rules));
    let bare = of(&[any(), veto("rm")]);
    assert!(kernel_table(&bare).is_none());
    assert_eq!(
        plan(&bare, Landlock::At(Abi::SCOPES)).expect("plans"),
        Some(Plan {
            handled: Access::NONE,
            scoped: Scoped::ENVELOPE
        })
    );
    assert_eq!(plan(&bare, Landlock::At(Abi(5))).expect("plans"), None);
    assert_eq!(
        plan(&bare, Landlock::Absent).expect("nothing to enforce"),
        None
    );
    let x = Tree::nowhere().path("/x");
    let cut = of(&[any(), deny(&x, false)]);
    assert!(kernel_table(&cut).is_some());
    assert!(kernel_table(&of(&[allow(&x, true)])).is_some());
}

/// A bind root inside an admitted directory is a root of its own, and the
/// exclusions lying inside it are subtracted along its spine; one the
/// table excludes whole is no root.
#[test]
fn a_sub_bind_is_expanded_from_its_own_root() {
    let (_tmp, d) = tree();
    let sub = real(d.join("s"));
    let rules = [allow(&d, true), deny(&d.join("s/f"), false)];
    let seen = expanded_on(&rules, std::slice::from_ref(&sub), true).expect("expands");
    assert_eq!(
        seen,
        admits(&d, &[("s/t", true)]),
        "the root `s`, less the file `s/f`, never `d` itself"
    );
    let rules = [allow(&d, true), deny(&d.join("s"), true)];
    let seen = expanded_on(&rules, &[sub], true).expect("expands");
    assert!(seen.is_empty(), "an excluded root admits nothing: {seen:?}");
}

/// An fs-denied directory is a scratch tmpfs the payload may `chmod` open
/// and write, so no exec rule covers it from above: the admitted directory
/// is lowered to rules around it, and a mask at the root admits nothing.
#[test]
fn a_masked_directory_under_an_admitted_one_is_covered_by_no_rule() {
    let (_tmp, d) = tree();
    let mask = |rel: &str| vec![real(d.join(rel))];
    let rules = [allow(&d, true)];
    let seen = expanded_masking(&rules, &whole(), false, &mask("s")).expect("expands");
    assert_eq!(
        seen,
        admits(
            &d,
            &[("a", false), ("x", false), ("n", false), ("sub", true)]
        ),
        "everything but `s`, which the walk never enters"
    );
    let seen = expanded_masking(&rules, &[real(d.join("s"))], false, &mask("s")).expect("expands");
    assert!(seen.is_empty(), "a masked root admits nothing: {seen:?}");
}

#[test]
fn moved_exclusions_are_judged_when_enumerated_and_at_exact_allowances() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let other = tmp.path().join("t");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&other).unwrap();
    std::fs::write(bin.join("ok"), "").unwrap();
    std::fs::write(other.join("evil"), "").unwrap();
    let exclusion = deny(&other.join("evil"), false);
    let trigger = deny(&bin.join("other"), false);
    let directory = allow(&bin, true);
    let exact = allow(&bin.join("x"), false);
    std::fs::rename(other.join("evil"), bin.join("x")).unwrap();
    let listed = expanded(&[directory.clone(), trigger, exclusion.clone()], true).unwrap();
    assert!(listed.contains(&(bin.join("ok"), false)), "{listed:?}");
    assert!(!listed.contains(&(bin.join("x"), false)), "{listed:?}");
    assert!(
        admitted_files(&[exact, exclusion.clone()])
            .unwrap()
            .is_empty()
    );
    let whole = expanded(&[directory, exclusion], true).unwrap();
    assert!(
        whole.contains(&(bin, true)),
        "unlisted directory residual: {whole:?}"
    );
}

#[test]
fn a_socket_write_grant_admits_only_its_opened_file() {
    use crate::authority::grant::freeze::FreezeCtx;
    use crate::authority::policy::Baseline;
    use crate::authority::sandbox::SandboxProjection;
    use crate::shell::runtime::Value;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("socket");
    let _server = UnixListener::bind(&path).unwrap();
    let record = |entries: Vec<(&str, Value)>| {
        Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        )
    };
    let grant = record(vec![(
        "fs",
        record(vec![(
            "write",
            Value::list(vec![Value::variant(
                "absolute",
                Some(Value::string(utf8(&path).unwrap())),
            )]),
        )]),
    )]);
    let ctx = FreezeCtx::at(
        crate::host::EnvVars::default(),
        crate::test_helper::process_cwd(),
    );
    let layer = crate::shell::grant::decode_capability_map(&grant, "test", &ctx).unwrap();
    let stack = crate::test_helper::stack([layer]);
    let projection = SandboxProjection::of(&stack, &Baseline::default(), None)
        .unwrap()
        .rendered()
        .unwrap();
    let mut seen = Vec::new();
    admit_sockets(&projection.fs, &whole(), &mut |admit| {
        let opened = rustix::fs::fstat(admit.fd)?;
        assert_eq!(FileType::from_raw_mode(opened.st_mode), FileType::Socket);
        assert_eq!(opened.st_ino, std::fs::symlink_metadata(&path)?.ino());
        seen.push(admit.path.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, [real(&path)]);
}
