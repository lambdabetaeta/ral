use super::*;

/// Where the stand-in's pin is locked: its current name.
fn pin_name() -> std::path::PathBuf {
    (stand_in().names())
        .expect("read the pin")
        .expect("the stand-in has a name")
}

fn prefix(path: &str) -> FsAllow {
    FsAllow::Prefix(
        GrantPath::<Trace>::walk(FrozenPath::from_surface(path)).expect("a prefix walks"),
    )
}

fn excluding(path: &str) -> GrantPath<Excluded> {
    GrantPath::<Excluded>::walk(FrozenPath::from_surface(path)).expect("an exclusion walks")
}

/// `policy`'s binds, opened over this host's tree, and the handoff
/// holding their handles.
fn lent(policy: &SandboxProjection, host: HostEnvelope) -> (Binds, Handoff) {
    let mut handoff = Handoff::default();
    let binds =
        Binds::open(&on(&stand_in(), host), plan(policy), &mut handoff).expect("the binds open");
    (binds, handoff)
}

/// `policy`'s binds, for a render, which reads their slots alone.
fn binds(policy: &SandboxProjection, host: HostEnvelope) -> Binds {
    lent(policy, host).0
}

/// What the descriptor lent at `slot` holds, while its handoff lives.
fn stat_of(slot: Slot) -> rustix::fs::Stat {
    rustix::fs::stat(crate::path::proc_fd_path(slot.fd())).expect("stat the lent handle")
}

/// What the handle lent for the bind at `dest` holds, while its
/// handoff lives.
fn held_at(binds: &Binds, dest: &std::path::Path) -> rustix::fs::Stat {
    let dest = dest.to_string_lossy();
    let (slot, _) = (binds.0.iter())
        .find(|(_, bind)| bind.dest.as_str() == dest)
        .expect("a bind there");
    stat_of(*slot)
}

/// The `--info-fd` slot a kept render names: a number, nothing lent.
fn info_slot() -> Slot {
    Slot::parse("98").expect("a slot")
}

/// A handoff lending `/dev/null` as the warrant: for a launch never
/// spawned, or refused before its trampoline would read one.
fn null_warrant() -> (Handoff, Slot) {
    use std::os::fd::AsFd;
    let null = std::fs::File::open("/dev/null").expect("open /dev/null");
    let mut handoff = Handoff::default();
    let at = handoff.lend(null.as_fd()).expect("lends");
    (handoff, at)
}

/// `policy` rendered over `binds`, kept or surrendered.
fn render(
    policy: &SandboxProjection,
    binds: &Binds,
    chdir: Option<&str>,
    ownership: Ownership,
    host: HostEnvelope,
) -> Vec<String> {
    let rendered = policy.rendered().expect("ASCII paths render");
    let address = (ownership == Ownership::Kept).then(info_slot);
    let chdir = chdir.map(std::path::Path::new);
    bwrap_argv(
        &rendered,
        chdir,
        address,
        host,
        binds,
        &GpuNodes::default(),
        &[],
    )
    .expect("ASCII paths render")
    .iter()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect()
}

fn options(
    policy: &SandboxProjection,
    chdir: Option<&str>,
    ownership: Ownership,
    host: HostEnvelope,
) -> Vec<String> {
    render(policy, &binds(policy, host), chdir, ownership, host)
}

fn options_on(host: HostEnvelope, policy: &SandboxProjection, ownership: Ownership) -> Vec<String> {
    options(policy, None, ownership, host)
}

fn options_for(policy: &SandboxProjection) -> Vec<String> {
    options_on(WHOLE, policy, Ownership::Kept)
}

fn position_of(args: &[String], mask: &[&str]) -> Option<usize> {
    args.windows(mask.len()).position(|w| w == mask)
}

/// Where `op` mounts a lent handle at `dest`, whatever its slot.
fn bound(args: &[String], op: &str, dest: &str) -> Option<usize> {
    (args.windows(3)).position(|w| w[0] == op && w[1].parse::<i32>().is_ok() && w[2] == dest)
}

#[test]
fn an_existing_file_is_masked_by_an_unopenable_node_after_its_bind() {
    let dir = workdir("deny-file");
    let denied = dir.join(".exarch.toml");
    std::fs::write(&denied, "capabilities").unwrap();

    let policy = deny_within(&dir, &[&denied]);
    let args = options_for(&policy);
    let bind = bound(&args, "--bind-fd", &dir.to_string_lossy());
    let mask = position_of(
        &args,
        &["--ro-bind", "/dev/null", &denied.to_string_lossy()],
    );

    assert!(bind.is_some(), "rw bind missing: {args:?}");
    assert!(
        mask.is_some(),
        "node mask missing for an existing file: {args:?}"
    );
    assert!(bind.unwrap() < mask.unwrap(), "the mask must win");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without `--perms` the mask is writable and a child's write inside it
/// succeeds into throwaway memory — a lie rather than a deny.
#[test]
fn a_directory_is_masked_by_a_tmpfs_with_no_permission_bits() {
    let dir = workdir("deny-dir");
    let denied = dir.join(".git");
    std::fs::create_dir_all(&denied).unwrap();

    let args = options_for(&deny_within(&dir, &[&denied]));
    assert!(
        position_of(
            &args,
            &["--perms", "0000", "--tmpfs", &denied.to_string_lossy()]
        )
        .is_some(),
        "a denied directory needs an unwritable tmpfs mask: {args:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// bwrap cannot `--chdir` into a name nothing mounted, so a cwd outside
/// the grant gets an empty unwritable directory — before the binds, which
/// hide it where they cover the name — and the root, the scratch `/tmp`
/// and an unrestricted envelope, which already have one, get none.
#[test]
fn a_cwd_outside_the_grant_is_stood_up_empty_and_unwritable() {
    let dir = workdir("cwd-stand-in");
    let dir_s = dir.to_string_lossy().into_owned();
    let granted = deny_within(&dir, &[]);
    for (label, policy, cwd, stood_up) in [
        ("ungranted", &granted, "/ral-ungranted/cwd", true),
        ("root", &granted, "/", false),
        ("scratch", &granted, "/tmp", false),
        ("unrestricted", &unrestricted(), "/ral-ungranted/cwd", false),
    ] {
        let args = options(policy, Some(cwd), Ownership::Kept, WHOLE);
        let mask = position_of(&args, &["--perms", "0555", "--tmpfs", cwd]);
        assert_eq!(mask.is_some(), stood_up, "{label}: {args:?}");
        if let Some(mask) = mask {
            let bind = bound(&args, "--bind-fd", &dir_s).expect("the grant's bind");
            assert!(
                mask < bind,
                "{label}: a bind must be able to cover it: {args:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// bwrap can neither `--chdir` into a mask nor pass through one, so a cwd
/// at or beneath a deny, named directly or through a link, is refused
/// before bwrap runs, naming both; a deny inside the cwd is ordinary.
#[test]
fn a_cwd_within_a_deny_is_refused_naming_both() {
    let dir = workdir("cwd-denied");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let denied = dir.join("secret");
    let inner = denied.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let link = dir.join("link");
    std::os::unix::fs::symlink(&denied, &link).unwrap();
    let argv = |deny: &std::path::Path, cwd: &std::path::Path| {
        let policy = deny_within(&dir, &[deny]);
        let rendered = policy.rendered().expect("ASCII paths render");
        let binds = binds(&policy, WHOLE);
        bwrap_argv(
            &rendered,
            Some(cwd),
            None,
            WHOLE,
            &binds,
            &GpuNodes::default(),
            &[],
        )
    };
    for (deny, cwd) in [(&denied, &denied), (&denied, &inner), (&link, &inner)] {
        let Err(why) = argv(deny, cwd) else {
            panic!(
                "{} within {} must be refused",
                cwd.display(),
                deny.display()
            );
        };
        assert!(
            why.contains(&*denied.to_string_lossy()) && why.contains(&*cwd.to_string_lossy()),
            "{why}"
        );
    }
    assert!(
        argv(&denied, &dir).is_ok(),
        "a deny inside the cwd must not refuse"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// One rule under either bind, for two reasons that meet in it: a mask is
/// a mount, a mount over an absent name needs a mountpoint bwrap must
/// `mkdir`, and that `mkdir` fails with `EROFS` under a read-only bind,
/// killing the envelope — the failure mode a denied-but-absent path like
/// `xdg:config/gcloud` triggers on a host with no gcloud — while under a
/// writable identity bind it succeeds on the host, and the deny creates
/// the name it forbids.
#[test]
fn an_absent_deny_is_never_mounted_over() {
    let dir = workdir("deny-absent");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    // Deliberately not created: the deny target must be absent.
    let under_write = dir.join("secret-not-yet-created");
    let under_read = config.join("gcloud");

    let read_only = restricted(&[&config], &[], &[&under_read]);
    for (bind, denied, policy) in [
        ("writable", &under_write, deny_within(&dir, &[&under_write])),
        ("read-only", &under_read, read_only),
    ] {
        let args = options_for(&policy);
        assert!(
            !args.contains(&denied.to_string_lossy().into_owned()),
            "no mount may land on an absent deny beneath a {bind} bind: {args:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Naming a symlink in the options costs the whole launch, so the mask goes
/// on the resolved twin — which the backend derives at render time, the
/// projection naming only the link.
#[test]
fn a_symlinked_deny_is_masked_at_its_target_and_never_at_the_link() {
    let dir = workdir("deny-link");
    let target = dir.join("id_rsa");
    let link = dir.join("link-to-id_rsa");
    std::fs::write(&target, "PRIVATE KEY").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let args = options_for(&deny_within(&dir, &[&link]));
    assert!(
        position_of(
            &args,
            &["--ro-bind", "/dev/null", &target.to_string_lossy()]
        )
        .is_some(),
        "the resolved target must carry the mask: {args:?}"
    );
    assert!(
        !args.iter().any(|a| *a == link.to_string_lossy()),
        "the link's own name must not be mounted over: {args:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The launched file is no bind of its own: law 7 has put it in the read
/// region, which shows it, and a grant that does not read it shows
/// nothing at its name.  The payload starts in the logical cwd.
#[test]
fn the_launched_file_is_shown_by_the_reads_alone_and_the_cwd_is_entered() {
    let (dir, real) = linked_tool("target-bind");
    let elsewhere = workdir("target-bind-elsewhere");
    let args = options(
        &deny_within(&elsewhere, &[]),
        Some("/"),
        Ownership::Kept,
        WHOLE,
    );
    assert!(
        bound(&args, "--ro-bind-fd", &real).is_none(),
        "an unread image is not shown: {args:?}"
    );
    assert!(
        !args
            .iter()
            .any(|a| a == "--symlink" || a.contains("linkdir")),
        "nothing inside looks the spelled name up: {args:?}"
    );
    assert!(
        position_of(&args, &["--chdir", "/"]).is_some(),
        "the payload must start in the logical cwd: {args:?}"
    );
    let args = options(&restricted(&[&dir], &[], &[]), None, Ownership::Kept, WHOLE);
    assert!(
        bound(&args, "--ro-bind-fd", &dir.to_string_lossy()).is_some()
            && bound(&args, "--ro-bind-fd", &real).is_none(),
        "a read of the directory shows it, and the image within it only there: {args:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// The `Shown` binds beneath `dir`, the baseline left out.
fn shown_within(binds: &Binds, dir: &std::path::Path) -> Vec<(String, Access)> {
    (binds.layer(Layer::Shown))
        .map(|(_, bind)| (bind.dest.as_str().to_string(), bind.access))
        .filter(|(dest, _)| std::path::Path::new(dest).starts_with(dir))
        .collect()
}

/// One bind per name the projection shows and another bind does not:
/// an absent prefix shows nothing; a read-only one within another bind,
/// or a run place within the grant's writable one, shows nothing; a
/// writable one within a read-only one wins; a file mounts as a file; a
/// run place outside every prefix is shown, as on macOS.
#[test]
fn a_bind_is_opened_for_each_name_no_other_bind_shows() {
    use std::os::unix::fs::MetadataExt;
    let dir = workdir("binds-open");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (ro, inner, w, file, tools) = (
        dir.join("ro"),
        dir.join("ro/inner"),
        dir.join("ro/w"),
        dir.join("file"),
        dir.join("tools"),
    );
    for at in [&inner, &w.join("bin"), &tools] {
        std::fs::create_dir_all(at).unwrap();
    }
    std::fs::write(&file, "x").unwrap();
    let absent = dir.join("absent");
    let mut policy = restricted(&[&ro, &inner, &file, &absent], &[&w], &[]);
    if let FsProjection::Restricted(rules) = &mut policy.fs {
        rules.read.extend(
            [&tools, &w.join("bin")].map(|d| FsAllow::RunPlace(crate::test_helper::real(d))),
        );
    }
    let (binds, _handoff) = lent(&policy, WHOLE);
    let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
    assert_eq!(
        shown_within(&binds, &dir),
        [
            (name(&file), Access::ReadOnly),
            (name(&ro), Access::ReadOnly),
            (name(&tools), Access::ReadOnly),
            (name(&w), Access::Writable),
        ]
    );
    let opened = held_at(&binds, &file);
    assert_eq!(opened.st_ino, std::fs::metadata(&file).unwrap().ino());
    assert!(rustix::fs::FileType::from_raw_mode(opened.st_mode).is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name through a symlink is shown at the object it reaches, and at
/// itself unless another bind shows the host's own link there.
#[test]
fn a_linked_prefix_shows_its_object_at_both_names() {
    let dir = workdir("binds-spelling");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (real, link) = (dir.join("real"), dir.join("link"));
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink("real", &link).unwrap();
    let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let both = binds(&restricted(&[&link], &[], &[]), WHOLE);
    assert_eq!(
        shown_within(&both, &dir),
        [
            (name(&link), Access::ReadOnly),
            (name(&real), Access::ReadOnly)
        ]
    );
    let covered = binds(&restricted(&[&dir, &link], &[], &[]), WHOLE);
    assert_eq!(
        shown_within(&covered, &dir),
        [(name(&dir), Access::ReadOnly)]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A writable bind beneath a symlinked ancestor is a mount of its own,
/// not the host's symlink: the read-only bind at the link does not cover
/// it.
#[test]
fn a_write_beneath_a_linked_ancestor_stays_writable() {
    let dir = workdir("binds-linked-ancestor");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (real, link) = (dir.join("real"), dir.join("link"));
    std::fs::create_dir_all(real.join("target")).unwrap();
    std::os::unix::fs::symlink("real", &link).unwrap();
    let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let binds = binds(&restricted(&[&link], &[&link.join("target")], &[]), WHOLE);
    assert_eq!(
        shown_within(&binds, &dir),
        [
            (name(&link), Access::ReadOnly),
            (name(&real), Access::ReadOnly),
            (name(&link.join("target")), Access::Writable),
            (name(&real.join("target")), Access::Writable),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writing does not withdraw run authority: a write that covers a run
/// place leaves the one writable bind as the whole of the view there.
#[test]
fn a_write_covering_a_run_place_leaves_one_writable_bind() {
    let dir = workdir("binds-write-over-place");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (w, bin) = (dir.join("w"), dir.join("w/bin"));
    std::fs::create_dir_all(&bin).unwrap();
    let mut policy = restricted(&[], &[&w], &[]);
    if let FsProjection::Restricted(rules) = &mut policy.fs {
        rules
            .read
            .push(FsAllow::RunPlace(crate::test_helper::real(&bin)));
    }
    policy.exec = ExecProjection::Restricted(
        std::iter::once(dir_rule(&crate::test_helper::real(&bin), true)).collect(),
    );
    let binds = binds(&policy, WHOLE);
    let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
    assert_eq!(shown_within(&binds, &w), [(name(&w), Access::Writable)]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A write prefix through a symlink is one handle bound at two names, and
/// a mount laid inside one bind does not show at the other: a mask goes
/// on at both.
#[test]
fn a_write_through_a_link_is_masked_at_both_its_names() {
    let dir = workdir("binds-linked-write");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (real, link) = (dir.join("real"), dir.join("link"));
    let secret = real.join("secret");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(&secret, "x").unwrap();
    std::os::unix::fs::symlink("real", &link).unwrap();
    let policy = restricted(&[], &[&link], &[&secret]);
    let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let args = options_for(&policy);
    for masked in [&secret, &link.join("secret")] {
        assert!(
            position_of(&args, &["--ro-bind", "/dev/null", &name(masked)]).is_some(),
            "{}: {args:?}",
            masked.display()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// bwrap starts the trampoline through the envelope's own `/proc`, so a
/// deny hiding it is refused before anything is built; one inside it is
/// a mask like any other.
#[test]
fn a_deny_that_hides_proc_is_refused() {
    for (denied, refused) in [("/proc", true), ("/proc/sys", false)] {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                deny: vec![excluding(denied)],
                ..FsRules::default()
            }),
            ..unrestricted()
        };
        let rendered = policy.rendered().expect("renders");
        let binds = binds(&policy, WHOLE);
        let argv = bwrap_argv(
            &rendered,
            None,
            None,
            WHOLE,
            &binds,
            &GpuNodes::default(),
            &[],
        );
        assert_eq!(argv.is_err(), refused, "{denied}: {argv:?}");
    }
}

/// A baseline path this user may not reach is left out, as an absent
/// one is; any other prefix the grant names is refused instead.
#[test]
fn an_unreachable_object_is_absent_only_where_no_grant_names_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workdir("binds-eacces");
    let locked = dir.join("locked");
    std::fs::create_dir_all(locked.join("inner")).unwrap();
    let inner = crate::test_helper::real(locked.join("inner"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let opened = super::super::open_object(&inner, false);
    if opened.is_ok() {
        eprintln!("skipping: this user reaches through a mode-0 directory");
    } else {
        assert!(opened.is_err_and(|why| why.contains("Permission denied")));
        assert!(matches!(super::super::open_object(&inner, true), Ok(None)));
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A grant renders real paths; a symlink on one when the launch opens it
/// was planted since, and refuses the launch rather than show its target.
#[test]
fn a_prefix_whose_component_became_a_symlink_since_render_is_refused() {
    let dir = workdir("binds-race");
    let (parent, elsewhere) = (dir.join("parent"), dir.join("elsewhere"));
    std::fs::create_dir_all(parent.join("data")).unwrap();
    std::fs::create_dir_all(elsewhere.join("data")).unwrap();
    let rendered = restricted(&[&parent.join("data")], &[], &[])
        .rendered()
        .expect("ASCII paths render");
    std::fs::rename(&parent, dir.join("moved")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &parent).unwrap();
    let shown = Shown::of(&rendered, &host_system_reads()).expect("the plan builds");
    let Err(why) = Binds::open(&on(&stand_in(), WHOLE), shown, &mut Handoff::default()) else {
        panic!("a symlinked component would show its target");
    };
    assert!(why.contains("symbolic link"), "{why}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory removed and made again after the grant rendered it is what
/// the launch mounts: the object at the position, not the one it named.
#[test]
fn a_directory_recreated_after_the_render_is_what_the_bind_holds() {
    use std::os::unix::fs::MetadataExt;
    let dir = workdir("binds-recreated");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let data = dir.join("data");
    std::fs::create_dir(&data).unwrap();
    let policy = restricted(&[&data], &[], &[]);
    let rendered = policy.rendered().expect("ASCII paths render");
    let before = std::fs::metadata(&data).unwrap().ino();
    std::fs::rename(&data, dir.join("old")).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::write(data.join("fresh"), "x").unwrap();
    let shown = Shown::of(&rendered, &host_system_reads()).expect("the plan builds");
    let mut handoff = Handoff::default();
    let binds = Binds::open(&on(&stand_in(), WHOLE), shown, &mut handoff).expect("the binds open");
    let held = held_at(&binds, &data);
    assert_ne!(held.st_ino, before, "the directory the grant named is gone");
    assert_eq!(held.st_ino, std::fs::metadata(&data).unwrap().ino());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A position that names a link when the launch opens it refuses the
/// launch, in the sentence it always did.
#[test]
fn a_position_turned_link_refuses_the_launch_in_todays_sentence() {
    let dir = workdir("binds-turned-link");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (data, elsewhere) = (dir.join("data"), dir.join("elsewhere"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let policy = restricted(&[&data], &[], &[]);
    let rendered = policy.rendered().expect("ASCII paths render");
    std::fs::rename(&data, dir.join("old")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &data).unwrap();
    let shown = Shown::of(&rendered, &host_system_reads()).expect("the plan builds");
    let Err(why) = Binds::open(&on(&stand_in(), WHOLE), shown, &mut Handoff::default()) else {
        panic!("a link at the position would show its target");
    };
    assert_eq!(
        why,
        format!(
            "sandbox: {} now goes through a symbolic link that ral will not follow; \
             reload the grant or restart ral if you made that change",
            data.display()
        )
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file allowance and a run place that is a file are bound as files.
#[test]
fn a_file_allowance_and_a_run_place_are_bound_as_files() {
    use std::os::unix::fs::MetadataExt;
    let dir = workdir("binds-files");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (allowed, tool) = (dir.join("allowed"), dir.join("tool"));
    for file in [&allowed, &tool] {
        std::fs::write(file, "x").unwrap();
    }
    let mut policy = restricted(&[&allowed], &[], &[]);
    if let FsProjection::Restricted(rules) = &mut policy.fs {
        let place = crate::test_helper::real(&tool);
        rules.read.push(FsAllow::RunPlace(place));
    }
    let (binds, _handoff) = lent(&policy, WHOLE);
    for file in [&allowed, &tool] {
        let held = held_at(&binds, file);
        assert!(rustix::fs::FileType::from_raw_mode(held.st_mode).is_file());
        assert_eq!(held.st_ino, std::fs::metadata(file).unwrap().ino());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every handle by its slot, in layer order: the projection's view, the
/// envelope's own `/proc` and `/dev`, what goes over everything (the
/// cgroup tree, then the pins at their current names), then the masks;
/// under an unrestricted fs the root, then `/proc`.  No source is a name
/// bwrap resolves but `/dev/null`.
#[test]
fn every_bind_is_a_handle_by_slot_in_layer_order() {
    let dir = workdir("argv-order");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (ro, w) = (dir.join("ro"), dir.join("ro/w"));
    let secret = w.join("secret");
    std::fs::create_dir_all(&w).unwrap();
    std::fs::write(&secret, "x").unwrap();
    let policy = restricted(&[&ro], &[&w], &[&secret]);
    let binds = binds(&policy, WHOLE);
    let args = render(&policy, &binds, None, Ownership::Kept, WHOLE);
    let at = |args: &[String], piece: &[&str]| {
        position_of(args, piece).unwrap_or_else(|| panic!("no {piece:?}: {args:?}"))
    };
    let by_handle = |args: &[String], op: &str, dest: &std::path::Path| {
        bound(args, op, &dest.to_string_lossy())
            .unwrap_or_else(|| panic!("no {op} at {}: {args:?}", dest.display()))
    };
    let order = [
        by_handle(&args, "--ro-bind-fd", &ro),
        by_handle(&args, "--bind-fd", &w),
        at(&args, &["--proc", "/proc"]),
        at(&args, &["--dev", "/dev"]),
        by_handle(&args, "--ro-bind-fd", std::path::Path::new(CGROUP)),
        by_handle(&args, "--ro-bind-fd", &pin_name()),
        at(
            &args,
            &["--ro-bind", "/dev/null", &secret.to_string_lossy()],
        ),
    ];
    assert!(order.is_sorted(), "{order:?}: {args:?}");
    let over_root = options_for(&unrestricted());
    let order = [
        at(&over_root, &["--dev-bind", "/", "/"]),
        at(&over_root, &["--proc", "/proc"]),
    ];
    assert!(order.is_sorted(), "{order:?}: {over_root:?}");
    let mounted: Vec<_> = (args.windows(3))
        .filter(|w| w[0] == "--ro-bind-fd" || w[0] == "--bind-fd")
        .map(|w| (w[1].clone(), w[2].as_str()))
        .collect();
    let expected: Vec<_> = (binds.layer(Layer::Shown))
        .chain(binds.layer(Layer::Over))
        .map(|(slot, bind)| (slot.to_string(), bind.dest.as_str()))
        .collect();
    assert_eq!(mounted, expected, "one slot per bind, in layer order");
    for w in args.windows(2).filter(|w| w[0] == "--ro-bind") {
        assert_eq!(w[1], "/dev/null", "a source bwrap would resolve by name");
    }
    assert!(
        !args.iter().any(|a| a == "--bind" || a == "--symlink"),
        "{args:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without `--ro-bind-fd` nothing could be shown but by name, so nothing
/// is launched at all; a bwrap that builds no envelope is refused in its
/// own words, never blamed on its version.
#[test]
fn a_bwrap_that_cannot_mount_by_descriptor_launches_nothing() {
    let rendered = unrestricted().rendered().expect("renders");
    let pin = stand_in();
    let path = crate::path::shown(pin.arg0());
    let refusal = |builds| {
        let (handoff, warrant) = null_warrant();
        let host = HostEnvelope { builds, ..WHOLE };
        let launch = bwrap_command(
            &on(&pin, host),
            plan(&unrestricted()),
            handoff,
            warrant,
            &rendered,
            None,
            Ownership::Kept,
        );
        launch
            .err()
            .expect("a launch it cannot mount must be refused")
            .to_string()
    };
    let why = refusal(Builds::ByName);
    assert!(why.contains(&path) && why.contains("0.8.0"), "{why}");
    let why = refusal(Builds::Nothing("No permissions to create a new namespace"));
    assert!(why.contains(&path) && why.contains("cannot build"), "{why}");
    assert!(
        why.contains("No permissions") && !why.contains("0.8.0"),
        "{why}"
    );
}

/// A grant keeps the real path it froze: a spelling retargeted since
/// shows the frozen object, never the new target.
#[test]
fn a_spelling_retargeted_after_freezing_shows_its_frozen_object() {
    let dir = workdir("binds-retarget");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (first, second, link) = (dir.join("first"), dir.join("second"), dir.join("link"));
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    std::os::unix::fs::symlink("first", &link).unwrap();
    let policy = restricted(&[&link], &[], &[]);
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("second", &link).unwrap();
    let shown = plan(&policy);
    let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let objects: Vec<_> = (shown.binds.iter())
        .map(|bind| bind.object.as_str().to_string())
        .filter(|object| std::path::Path::new(object).starts_with(&dir))
        .collect();
    assert!(objects.contains(&name(&first)), "{objects:?}");
    assert!(!objects.contains(&name(&second)), "{objects:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fallback is the same `/dev`, and only where the host refuses one.
#[test]
fn a_host_that_refuses_dev_gets_the_same_shape_by_hand() {
    let dir = workdir("dev-render");
    let policy = deny_within(&dir, &[]);

    let mounted = options_for(&policy);
    assert!(
        position_of(&mounted, &["--dev", "/dev"]).is_some(),
        "a host that mounts --dev must be given it: {mounted:?}"
    );

    let by_hand = options_on(
        HostEnvelope {
            virtual_dev: false,
            ..WHOLE
        },
        &policy,
        Ownership::Kept,
    );
    assert!(
        position_of(&by_hand, &["--dev", "/dev"]).is_none(),
        "the refused mount must not be asked for: {by_hand:?}"
    );
    let dev = position_of(&by_hand, &["--tmpfs", "/dev"]).expect("a /dev to fill");
    let shm = position_of(&by_hand, &["--tmpfs", "/dev/shm"]).expect("a fresh /dev/shm");
    assert!(dev < shm, "/dev's tmpfs would bury /dev/shm's: {by_hand:?}");
    for piece in [
        ["--dev-bind", "/dev/null", "/dev/null"],
        ["--dev-bind", "/dev/urandom", "/dev/urandom"],
        ["--symlink", "pts/ptmx", "/dev/ptmx"],
        ["--symlink", "/proc/self/fd/1", "/dev/stdout"],
    ] {
        assert!(
            position_of(&by_hand, &piece).is_some(),
            "the by-hand /dev is missing {piece:?}: {by_hand:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A host's GPU as discovery would find it.
fn gpu() -> GpuNodes {
    GpuNodes {
        dri: true,
        render: vec!["/dev/dri/renderD128".into()],
        nodes: vec!["/dev/nvidia0".into(), "/dev/nvidiactl".into()],
        devices: vec!["/sys/devices/pci0000:00/0000:00:02.0".into()],
    }
}

fn render_gpu(policy: &SandboxProjection) -> Vec<String> {
    let rendered = policy.rendered().expect("ASCII paths render");
    bwrap_argv(
        &rendered,
        None,
        None,
        WHOLE,
        &binds(policy, WHOLE),
        &gpu(),
        &[],
    )
    .expect("ASCII paths render")
    .iter()
    .map(|arg| arg.to_string_lossy().into_owned())
    .collect()
}

/// Left on, the GPU enters the envelope's own `/dev` and `/sys`, never a
/// `card*`; withheld, none of it does.
#[test]
fn a_restricted_envelope_shows_the_gpu_only_where_left_on() {
    let dir = workdir("gpu");
    let mut policy = restricted(&[], &[&dir], &[]);
    let on = render_gpu(&policy);
    let dev = position_of(&on, &["--dev", "/dev"]).expect("the envelope's /dev");
    let node = position_of(
        &on,
        &[
            "--dev-bind-try",
            "/dev/dri/renderD128",
            "/dev/dri/renderD128",
        ],
    )
    .expect("the render node");
    assert!(dev < node, "{on:?}");
    for piece in [
        ["--dev-bind-try", "/dev/nvidiactl", "/dev/nvidiactl"],
        ["--ro-bind-try", "/sys/class/drm", "/sys/class/drm"],
        [
            "--ro-bind-try",
            "/sys/devices/pci0000:00/0000:00:02.0",
            "/sys/devices/pci0000:00/0000:00:02.0",
        ],
    ] {
        assert!(position_of(&on, &piece).is_some(), "{piece:?}: {on:?}");
    }
    assert!(!on.iter().any(|a| a.contains("card")), "{on:?}");
    policy.gpu = false;
    let off = render_gpu(&policy);
    assert!(
        !off.iter()
            .any(|a| a.contains("renderD") || a.contains("nvidia") || a.contains("/sys/class/drm")),
        "{off:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Over the host's `/dev`, a withheld GPU is masked; left on, it is untouched.
#[test]
fn an_unrestricted_envelope_masks_a_withheld_gpu() {
    let mut policy = unrestricted();
    let on = render_gpu(&policy);
    assert!(
        !on.iter()
            .any(|a| a.contains("nvidia") || a.contains("/dev/dri")),
        "{on:?}"
    );
    policy.gpu = false;
    let off = render_gpu(&policy);
    let root = position_of(&off, &["--dev-bind", "/", "/"]).expect("the host's root");
    let dri =
        position_of(&off, &["--perms", "0000", "--tmpfs", "/dev/dri"]).expect("/dev/dri emptied");
    assert!(root < dri, "{off:?}");
    for node in ["/dev/nvidia0", "/dev/nvidiactl"] {
        assert!(
            position_of(&off, &["--ro-bind", "/dev/null", node]).is_some(),
            "{node}: {off:?}"
        );
    }
}

/// The namespaces key on there being an envelope; the pid and cgroup ones
/// on the host fact each.
#[test]
fn every_envelope_owns_its_namespaces_and_the_probed_ones_where_the_host_allows() {
    let restricted = deny_within(&workdir("namespaces"), &[]);
    let masked = HostEnvelope {
        private_pids: false,
        private_cgroup: false,
        ..WHOLE
    };
    for (host, probed) in [(WHOLE, true), (masked, false)] {
        for policy in [&unrestricted(), &restricted] {
            for ownership in [Ownership::Kept, Ownership::Surrendered] {
                let args = options_on(host, policy, ownership);
                for flag in ["--unshare-ipc", "--unshare-uts"] {
                    assert!(
                        args.iter().any(|a| a == flag),
                        "{flag} must be on every envelope: {args:?}"
                    );
                }
                for flag in ["--unshare-pid", "--unshare-cgroup"] {
                    assert_eq!(
                        args.iter().any(|a| a == flag),
                        probed,
                        "{flag} must follow the host fact alone: {args:?}"
                    );
                }
            }
        }
    }
}

/// `/sys` describes the host — `class/net` is the mounter's netns whatever
/// `--unshare-net` did — so only what sizes a program is bound, and the
/// cgroup tree goes on last as the payload's own: under the namespace its
/// `/proc/self/cgroup` reads `0::/`, and a tree rooted anywhere else
/// answers that path with somebody else's limits.
#[test]
fn sys_is_narrowed_to_what_sizes_a_program_and_the_cgroup_tree_is_the_payloads() {
    use std::os::unix::fs::MetadataExt;
    let dir = workdir("sys");
    let dir_s = dir.to_string_lossy();
    let own = format!(
        "{CGROUP}{}",
        super::super::own_cgroup().expect("a cgroup2 host")
    );
    // The tree's handle, by the inode it opened.
    let tree = |host: HostEnvelope, policy: &SandboxProjection| {
        let (binds, _handoff) = lent(policy, host);
        let (slot, bind) = (binds.0.iter())
            .find(|(_, bind)| bind.dest.as_str() == CGROUP)
            .expect("a cgroup bind");
        assert_eq!(bind.layer, Layer::Over);
        stat_of(*slot).st_ino
    };
    let ino = |path: &str| std::fs::metadata(path).expect("the tree exists").ino();
    for (label, policy) in [
        ("unrestricted", unrestricted()),
        ("restricted", deny_within(&dir, &[])),
    ] {
        let args = options_for(&policy);
        assert!(
            bound(&args, "--ro-bind-fd", "/sys").is_none(),
            "{label}: the host's /sys must not be bound wholesale: {args:?}"
        );
        let projection = position_of(&args, &["--dev-bind", "/", "/"])
            .or_else(|| bound(&args, "--bind-fd", &dir_s))
            .expect("the projection's bind");
        let cgroup = bound(&args, "--ro-bind-fd", CGROUP)
            .unwrap_or_else(|| panic!("{label}: no cgroup tree: {args:?}"));
        assert!(
            projection < cgroup,
            "{label}: the tree goes over the projection's binds: {args:?}"
        );
        assert_eq!(tree(WHOLE, &policy), ino(&own), "{label}: not re-rooted");
    }
    let restricted = options_for(&deny_within(&dir, &[]));
    assert!(
        bound(&restricted, "--ro-bind-fd", "/sys/devices/system/cpu").is_some(),
        "a program must still count its cpus: {restricted:?}"
    );
    let shared = HostEnvelope {
        private_cgroup: false,
        ..WHOLE
    };
    assert_eq!(
        tree(shared, &deny_within(&dir, &[])),
        ino(CGROUP),
        "without the namespace the host's tree is the true one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bound root, or a prefix of `/`, carries the host's `/proc` and
/// `/dev`; the envelope's own must land after it, or `HostEnvelope`
/// reports a hidden table that is the host's.  `/tmp` goes the other
/// way: a write prefix beneath it lands inside the tmpfs.
#[test]
fn the_fresh_proc_and_dev_go_over_every_projection_bind() {
    let dir = workdir("proc-over-binds");
    let dir_s = dir.to_string_lossy().into_owned();
    let rooted = SandboxProjection {
        fs: FsProjection::Restricted(FsRules {
            read: vec![prefix("/")],
            write: vec![prefix(&dir_s)],
            ..FsRules::default()
        }),
        net: true,
        gpu: true,
        exec: ExecProjection::default(),
    };
    // `Unrestricted` dev-binds `/` wholesale and mounts no `/dev` of its own.
    for (label, policy, binds, fresh) in [
        (
            "unrestricted",
            unrestricted(),
            vec![["--dev-bind", "/"]],
            vec![["--proc", "/proc"]],
        ),
        (
            "restricted",
            rooted,
            vec![["--ro-bind-fd", "/"], ["--bind-fd", dir_s.as_str()]],
            vec![["--proc", "/proc"], ["--dev", "/dev"], ["--tmpfs", "/tmp"]],
        ),
    ] {
        let args = options_for(&policy);
        let at = |&[op, dest]: &[&str; 2]| {
            match op {
                "--ro-bind-fd" | "--bind-fd" => bound(&args, op, dest),
                "--dev-bind" => position_of(&args, &[op, dest, dest]),
                _ => position_of(&args, &[op, dest]),
            }
            .unwrap_or_else(|| panic!("{label}: no {op} {dest} in the options: {args:?}"))
        };
        for bind in &binds {
            for mount in &fresh {
                let ordered = if mount[1] == "/tmp" {
                    at(mount) < at(bind)
                } else {
                    at(bind) < at(mount)
                };
                assert!(
                    ordered,
                    "{label}: {bind:?} and {mount:?} are in the wrong order: {args:?}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The launcher is the file pinned at boot, named by descriptor: no
/// `PATH` — the shell's override included — has any say in what runs.
/// Its own argv names, each by the slot it was lent at, the parcel its
/// options arrive in, the trampoline — ral's pin — and the warrant.
#[test]
fn the_launcher_is_the_pinned_envelope_and_never_a_name() {
    let pin = stand_in();
    let (handoff, warrant) = null_warrant();
    let (cmd, _info_fd) = bwrap_command(
        &on(&pin, WHOLE),
        plan(&unrestricted()),
        handoff,
        warrant,
        &unrestricted().rendered().expect("renders"),
        None,
        Ownership::Kept,
    )
    .expect("ASCII paths render");
    let program = cmd.get_program().to_string_lossy();
    assert!(
        program.starts_with("/proc/self/fd/"),
        "the envelope must be exec'd by pinned descriptor: {program}"
    );
    let args: Vec<_> = (cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let [opt, options, dashes, trampoline, flag, at] = &args[..] else {
        panic!("not `bwrap --args <a> -- /proc/self/fd/<t> --warrant <w>`: {args:?}");
    };
    assert_eq!(
        [opt.as_str(), dashes, flag, at],
        ["--args", "--", Role::Warrant.flag(), &warrant.to_string()],
        "{args:?}"
    );
    let options = Slot::parse(options).expect("`--args` names a slot");
    let parcel = std::fs::read(crate::path::proc_fd_path(options.fd()))
        .expect("the options' parcel is open");
    assert!(parcel.starts_with(b"--new-session\0"), "{parcel:?}");
    let trampoline = (trampoline.strip_prefix("/proc/self/fd/"))
        .and_then(|at| Slot::parse(at).ok())
        .expect("the trampoline by its slot");
    let lent = stat_of(trampoline);
    let pinned = rustix::fs::fstat(pin.fd()).expect("fstat the pin");
    assert_eq!(
        (lent.st_dev, lent.st_ino),
        (pinned.st_dev, pinned.st_ino),
        "the trampoline must be ral's pin"
    );
}

/// Whatever the projection bound, both files a launch execs go read-only
/// over it by their own descriptors at their current names, before the
/// masks.  The two names coincide here, the stand-in envelope and
/// trampoline both being this test binary, so one bind locks both.
#[test]
fn the_envelope_binary_is_read_only_inside_every_envelope() {
    let pin = pin_name();
    let pin = pin.to_string_lossy();
    let dir = workdir("envelope-ro");
    let dir_s = dir.to_string_lossy();
    let denied = dir.join("secret");
    std::fs::write(&denied, "x").unwrap();

    for (label, policy) in [
        ("unrestricted", unrestricted()),
        ("restricted", deny_within(&dir, &[&denied])),
    ] {
        let args = options_for(&policy);
        let bound_at = position_of(&args, &["--dev-bind", "/", "/"])
            .or_else(|| bound(&args, "--bind-fd", &dir_s))
            .expect("the projection's bind");
        let ro = bound(&args, "--ro-bind-fd", &pin)
            .unwrap_or_else(|| panic!("{label}: no read-only bind of the pins: {args:?}"));
        assert!(bound_at < ro, "{label}: the pins' bind must win: {args:?}");
        assert_eq!(args.iter().filter(|a| **a == *pin).count(), 1, "{label}");
        if let Some(mask) = position_of(
            &args,
            &["--ro-bind", "/dev/null", &denied.to_string_lossy()],
        ) {
            assert!(ro < mask, "{label}: masks go on last: {args:?}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A kept child's envelope is tied to our death and a surrendered one's
/// must not be, while every other confinement flag stays identical.
#[test]
fn only_the_two_ownership_ties_distinguish_a_surrendered_launch() {
    let policy = SandboxProjection {
        fs: FsProjection::Restricted(FsRules {
            read: vec![prefix("/usr")],
            ..FsRules::default()
        }),
        net: false,
        gpu: true,
        exec: ExecProjection::default(),
    };
    let binds = binds(&policy, WHOLE);
    let kept = render(&policy, &binds, None, Ownership::Kept, WHOLE);
    let surrendered = render(&policy, &binds, None, Ownership::Surrendered, WHOLE);

    assert!(
        kept.contains(&"--die-with-parent".to_string()),
        "a child we keep must not outlive us: {kept:?}"
    );
    let info = info_slot().to_string();
    assert!(
        kept.windows(2).any(|w| w == ["--info-fd", info.as_str()]),
        "a kept launch must carry an `--info-fd` naming its payload's session: {kept:?}"
    );
    assert!(
        !surrendered.contains(&"--die-with-parent".to_string()),
        "a survivor must not be killed by our death: {surrendered:?}"
    );
    assert!(
        !surrendered.contains(&"--info-fd".to_string()),
        "a surrendered launch has no session left here to address: {surrendered:?}"
    );
    let mut untied = kept;
    let at = (untied.iter().position(|a| a == "--info-fd")).expect("an --info-fd");
    untied.drain(at..at + 2);
    untied.retain(|a| a != "--die-with-parent");
    assert_eq!(
        untied, surrendered,
        "the two launches must otherwise be confined identically"
    );
}
