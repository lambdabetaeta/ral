use super::{FsAllow, FsRules, ProjectionKey, profile_name, refuse_deny_meeting_grant};
use crate::authority::policy::GrantPath;
use crate::authority::sandbox::Refusal;
use crate::authority::sandbox::carve::{Carving, carve};
use crate::path::{Excluded, FrozenPath, RealPath, Trace};
use crate::test_helper::{place, real as walked};
use std::path::{Path, PathBuf};

#[test]
fn projection_key_identity_is_the_fs_triple() {
    let a = ProjectionKey {
        read: vec![r"C:\work".into()],
        write: vec![r"C:\work\out".into()],
        deny: vec![],
    };
    let same = a.clone();
    let narrower = ProjectionKey {
        read: vec![r"C:\work".into()],
        write: vec![],
        deny: vec![],
    };
    let denied = ProjectionKey {
        deny: vec![r"C:\work\secret".into()],
        ..a.clone()
    };
    assert_eq!(a, same);
    assert_ne!(a, narrower);
    assert_ne!(a, denied);
}

fn prefixes(paths: &[&str]) -> Vec<FsAllow> {
    paths
        .iter()
        .map(|p| {
            FsAllow::Prefix(
                GrantPath::<Trace>::walk(FrozenPath::from_surface(p)).expect("a prefix walks"),
            )
        })
        .collect()
}

fn rules(read: &[&str], write: &[&str], deny: &[&str]) -> FsRules {
    FsRules {
        read: prefixes(read),
        write: prefixes(write),
        deny: deny
            .iter()
            .map(|d| {
                GrantPath::<Excluded>::walk(FrozenPath::from_surface(d))
                    .expect("an exclusion walks")
            })
            .collect(),
        pinned_dirs: Vec::new(),
    }
}

/// The process-global session, torn down when the test ends — unwinding
/// included, so a failed test leaves no profiles, projections or ledger
/// entries for the next one to inherit.
struct SessionGuard;

impl Drop for SessionGuard {
    fn drop(&mut self) {
        super::teardown();
    }
}

/// Both regions' positions, as `confine` asks them.
fn refuse_universal_prefix(rules: &FsRules) -> Result<(), Refusal> {
    super::positions(&rules.read, rules)?;
    super::positions(&rules.write, rules)?;
    Ok(())
}

#[test]
fn the_universal_prefix_is_refused_in_either_region() {
    for rules in [
        rules(&["/"], &[], &[r"C:\secret"]),
        rules(&[], &["/"], &[r"C:\secret"]),
        rules(&[r"C:\work"], &["/"], &[]),
    ] {
        let why = refuse_universal_prefix(&rules).unwrap_err().to_string();
        assert!(why.contains("ral will not stamp every drive"), "{why}");
    }
    let why = refuse_universal_prefix(&rules(&["/"], &[], &[r"C:\secret"]))
        .unwrap_err()
        .to_string();
    assert!(why.contains("every path except some exclusions"), "{why}");
}

#[test]
fn a_drive_root_is_not_the_universal_prefix() {
    assert!(refuse_universal_prefix(&rules(&[r"C:"], &[r"C:\work"], &[])).is_ok());
}

/// The positions of the `/`-rooted fixture names, spelled as given: places
/// nothing occupies, so a name keeps its case.
fn positions(paths: &[&str]) -> Vec<RealPath> {
    let nowhere = crate::test_helper::Tree::nowhere();
    paths.iter().map(|p| place(nowhere.path(p))).collect()
}

/// What carving leaves — an allowance on or inside an exclusion, the program
/// image among them — is refused, under every spelling a filesystem could take
/// for the same name; the invariant is stated both ways.
#[test]
fn an_exclusion_meeting_a_whole_grant_is_refused() {
    for (granted, denied) in [
        ("/work", "/work"),
        ("/secret/run", "/secret"),
        ("/secret/tool.exe", "/Secret"),
        ("/work", "/WORK/secret"),
    ] {
        let granted = positions(&[granted]);
        let refused =
            refuse_deny_meeting_grant(&granted.iter().collect::<Vec<_>>(), &positions(&[denied]));
        match refused {
            Err(Refusal::Unavailable(why)) => {
                assert!(why.contains("cannot enforce together"), "{why}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

/// An exclusion apart from every whole grant is the shape Windows can keep. A
/// sibling sharing a name prefix is apart.
#[test]
fn an_exclusion_apart_from_every_whole_grant_is_kept() {
    let granted = positions(&["/work", "/out", "/tools/tool.exe"]);
    let granted: Vec<&RealPath> = granted.iter().collect();
    for denied in ["/secret", "/work-secret"] {
        assert!(refuse_deny_meeting_grant(&granted, &positions(&[denied])).is_ok());
    }
}

#[test]
fn a_redirected_allowance_refuses_the_launch_before_the_memo_is_consulted() {
    use crate::authority::sandbox::{ExecProjection, FsProjection, SandboxProjection};
    use crate::process::Launch;
    use crate::process::cancel::DurableRoot;
    let _env = crate::host::env::test_support::env_guard();
    let root = tempfile::tempdir().unwrap();
    let (granted, outside) = (root.path().join("granted"), root.path().join("outside"));
    std::fs::create_dir(&granted).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let projection = SandboxProjection {
        fs: FsProjection::Restricted(rules(&[granted.to_str().unwrap()], &[], &[])),
        net: false,
        exec: ExecProjection::default(),
    };
    let _session = SessionGuard;
    let confine = || {
        let mut launch = Launch::new(r"C:\Windows\System32\cmd.exe");
        super::confine(
            &mut launch,
            &projection,
            None,
            &DurableRoot::default().worker(),
        )
    };
    confine().expect("a real directory is granted");
    std::fs::remove_dir(&granted).unwrap();
    let junction = std::process::Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&granted)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(junction.status.success(), "{junction:?}");
    let why = confine().unwrap_err().to_string();
    assert!(
        why.contains("an allowance names what it named when granted"),
        "{why}"
    );
}

/// A tree holding a junction and a symlink is carved by listing it without
/// following either: each is a link, never granted, and what they lead to is
/// neither listed nor granted through them.
#[test]
fn carving_meets_a_junction_and_a_symlink_as_links() {
    let td = tempfile::tempdir().unwrap();
    let root = walked(td.path());
    let (cfg, outside) = (root.as_path().join("cfg"), root.as_path().join("outside"));
    for dir in [cfg.join("gh"), cfg.join("keep"), outside.join("inner")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(cfg.join("other.txt"), b"x").unwrap();
    let junction = std::process::Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(cfg.join("junction"))
        .arg(&outside)
        .output()
        .unwrap();
    assert!(junction.status.success(), "{junction:?}");
    std::os::windows::fs::symlink_dir(&outside, cfg.join("symlink")).unwrap();

    let real = |at: PathBuf| walked(&at);
    let allowance = real(cfg.clone());
    let mut carved = carve(std::slice::from_ref(&allowance), &[real(cfg.join("gh"))]).unwrap();
    carved.whole.sort();
    let mut whole = vec![real(cfg.join("keep")), real(cfg.join("other.txt"))];
    whole.sort();
    assert_eq!(
        carved,
        Carving {
            whole,
            entries: vec![allowance]
        }
    );
}

/// End-to-end proof that the kernel honors a *private, derived*
/// capability SID: `CreateProcessW` must accept it inside
/// `SECURITY_CAPABILITIES`, the stamped allow-ACE must admit a write
/// inside the granted prefix (the positive control, load-bearing against
/// a vacuous denial), and deny-by-default must hold a write outside it.
///
/// Runs against the real user state dir under the shared env guard, so a
/// `dacl` test's scoped `XDG_STATE_HOME` can never point this test's
/// stamp writes at a deleted tempdir. What it leaves behind is one
/// stamp-store key naming a dead tempdir, inert.
#[test]
fn confined_child_writes_inside_the_grant_and_not_outside() {
    let _env = crate::host::env::test_support::env_guard();
    use crate::authority::sandbox::{ExecProjection, FsProjection, SandboxProjection};
    use crate::process::cancel::DurableRoot;
    use crate::process::{Launch, PgidPolicy, StdioSpec, WaitOutcome};
    use std::path::Path;

    // Siblings under one root, so the child can name the ungranted one
    // relative to the granted cwd it runs in — see the quoting note below.
    let root = tempfile::tempdir().unwrap();
    let granted = root.path().join("granted");
    let outside = root.path().join("outside");
    std::fs::create_dir(&granted).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let projection = SandboxProjection {
        fs: FsProjection::Restricted(rules(
            &[granted.to_str().unwrap()],
            &[granted.to_str().unwrap()],
            &[],
        )),
        net: false,
        exec: ExecProjection::default(),
    };
    let _session = SessionGuard;

    // `cmd.exe` loads via the ALL APPLICATION PACKAGES system grants, so
    // the only filesystem authority under test is the derived
    // capability's. Redirection creates the file directly; `copy NUL`
    // would additionally require access to a global DOS device.
    //
    // The `/c` string must carry no quote character, and so names its
    // target relative to the granted cwd the child runs in. That is not
    // tidiness: `CreateProcessW` takes one flat command line, so an
    // embedded `"` leaves this process escaped as `\"`: and `cmd.exe`,
    // which has no backslash escape, would then redirect into a filename
    // beginning with a backslash and a quote, fail its own syntax check,
    // and exit nonzero having never asked the filesystem anything. A bare
    // relative name admits no such reading, which is what leaves the exit
    // status below able to speak: the only way this child can now fail is
    // that the write was refused.
    //
    // Both arms report how the child *ended*, not just whether the file
    // appeared: a refused write and a child that never reached the write
    // leave the same empty directory, and only the exit status tells a
    // reader on another machine which of the two it is looking at.
    let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
    let image = walked(cmd);
    let run_echo_to = |rel: &str| -> WaitOutcome {
        let mut launch = Launch::new(cmd);
        launch.args(["/d", "/s", "/c", &format!("echo ok>{rel}")]);
        launch.current_dir(&granted);
        launch.stdin(StdioSpec::null());
        launch.stdout(StdioSpec::null());
        launch.stderr(StdioSpec::null());
        super::confine(
            &mut launch,
            &projection,
            Some(&image),
            &DurableRoot::default().worker(),
        )
        .expect("confine under a derived capability SID");
        let (mut child, _pgid, _) = launch
            .spawn(PgidPolicy::Inherit)
            .expect("CreateProcessW must accept the derived capability SIDs");
        let status = child.reap().expect("wait for the confined child");
        WaitOutcome::from_exit_status(status)
    };

    let ended_inside = run_echo_to("ok.txt");
    assert!(
        granted.join("ok.txt").exists(),
        "write inside the granted prefix must land, and did not; the child \
         ended {ended_inside:?}. A nonzero exit means it ran and the write \
         was refused: the stamped capability ACE or the Low mandatory \
         label failed to admit it. An exit that is neither 0 nor 1 means \
         the child never reached the write at all, and the grant is not \
         what is under test."
    );
    let ended_outside = run_echo_to(r"..\outside\nope.txt");
    assert!(
        !outside.join("nope.txt").exists(),
        "write outside the grant must be held by deny-by-default; the \
         child ended {ended_outside:?}"
    );
}

/// Run `f` with the stamp store under its own `XDG_STATE_HOME`, so these
/// stamps never land in the user's state or in another test's.
fn with_scoped_state(f: impl FnOnce()) {
    let state = tempfile::tempdir().unwrap();
    crate::host::env::test_support::with_var(
        "XDG_STATE_HOME",
        Some(state.path().to_str().unwrap()),
        f,
    );
}

/// The paths beneath `A` that a read grant on `A` reaches, each holding
/// `secret`: one by inheritance alone, two behind a protected DACL that copied
/// the grant down, one with an explicit ACE of its own.
const READABLE: &[&str] = &[
    "ok.txt",
    r"D\open.txt",
    r"D\protected\key.txt",
    r"D\protected\nested\key.txt",
    r"D\protected-file.txt",
    r"D\explicit\key.txt",
];

/// `D\dark`, whose inheritance is cut off, so no grant above it reaches it.
const DARK: &str = r"D\dark\key.txt";

/// The `ALL APPLICATION PACKAGES` SID: an ACE naming it admits every sandbox.
const ALL_APPLICATION_PACKAGES: &str = "S-1-15-2-1";

fn icacls(path: &Path, args: &[&str]) {
    let output = std::process::Command::new(r"C:\Windows\System32\icacls.exe")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "icacls {path:?} {args:?}: {output:?}"
    );
}

/// A temporary root holding the granted `A` and the shapes beneath it.
struct Tree {
    root: tempfile::TempDir,
}

impl Tree {
    fn new() -> Self {
        let tree = Self {
            root: tempfile::tempdir().unwrap(),
        };
        for relative in READABLE.iter().copied().chain([DARK]) {
            tree.write(&format!(r"A\{relative}"));
        }
        tree
    }

    /// `relative` under the root, created holding `secret`.
    fn write(&self, relative: &str) -> PathBuf {
        let file = self.at(relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"secret\r\n").unwrap();
        file
    }

    /// `relative` under the root, in the ordinary drive spelling `cmd.exe`
    /// needs.
    fn at(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    /// The real path the grants key on.
    fn real(&self, relative: &str) -> String {
        crate::path::shown(crate::test_helper::real(self.at(relative)).as_path())
    }

    /// Cut `D`'s descendants off from inheritance in each way a DACL can.
    fn shape(&self) {
        for relative in [
            r"A\D\protected",
            r"A\D\protected\nested",
            r"A\D\protected-file.txt",
        ] {
            icacls(&self.at(relative), &["/inheritance:d"]);
        }
        let dark = self.at(r"A\D\dark");
        // OWNER RIGHTS keeps the owner's access once inherited entries go.
        icacls(&dark, &["/grant", "*S-1-3-4:(OI)(CI)F"]);
        icacls(&dark, &["/inheritance:r"]);
        icacls(
            &self.at(r"A\D\explicit\key.txt"),
            &["/grant", &format!("*{ALL_APPLICATION_PACKAGES}:(RX)")],
        );
    }
}

fn projection(read: &[&str], deny: &[&str]) -> crate::authority::sandbox::SandboxProjection {
    crate::authority::sandbox::SandboxProjection {
        fs: crate::authority::sandbox::FsProjection::Restricted(rules(read, &[], deny)),
        net: false,
        exec: crate::authority::sandbox::ExecProjection::default(),
    }
}

fn cmd() -> &'static Path {
    Path::new(r"C:\Windows\System32\cmd.exe")
}

/// `confine` a `cmd.exe` launch under `projection`, from `cwd`.
fn confined(
    projection: &crate::authority::sandbox::SandboxProjection,
    cwd: &Path,
    line: &str,
) -> Result<crate::process::Launch, Refusal> {
    confined_as(projection, cwd, line, &walked(cmd()))
}

/// [`confined`], with `image` handed to `confine` as the program's image.
fn confined_as(
    projection: &crate::authority::sandbox::SandboxProjection,
    cwd: &Path,
    line: &str,
    image: &RealPath,
) -> Result<crate::process::Launch, Refusal> {
    use crate::process::cancel::DurableRoot;
    use crate::process::{Launch, StdioSpec};
    let mut launch = Launch::new(cmd());
    launch.args(["/d", "/s", "/c", line]);
    launch.current_dir(cwd);
    launch.stdin(StdioSpec::null());
    launch.stdout(StdioSpec::Piped);
    launch.stderr(StdioSpec::Piped);
    super::confine(
        &mut launch,
        projection,
        Some(image),
        &DurableRoot::default().worker(),
    )?;
    Ok(launch)
}

/// Run `type relative` confined under `projection` from `cwd`, and assert it
/// reads `secret` exactly when `readable`.
fn assert_reads(
    projection: &crate::authority::sandbox::SandboxProjection,
    cwd: &Path,
    relative: &str,
    readable: bool,
) {
    let launch = confined(projection, cwd, &format!("type {relative}")).unwrap();
    assert_launch_reads(launch, relative, readable);
}

/// Spawn `launch`, a confined `type relative`, and assert it reads `secret`
/// exactly when `readable`.
fn assert_launch_reads(mut launch: crate::process::Launch, relative: &str, readable: bool) {
    use crate::process::PgidPolicy;
    use std::io::Read;
    let (mut child, _pgid, _) = launch.spawn(PgidPolicy::Inherit).unwrap();
    let mut stdout = Vec::new();
    child
        .take_stdout()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let mut stderr = Vec::new();
    child
        .take_stderr()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    let ended = child.reap().unwrap();
    assert_eq!(
        ended.code(),
        Some(i32::from(!readable)),
        "{relative}: {ended:?}, stderr={}",
        String::from_utf8_lossy(&stderr)
    );
    let expected: &[u8] = if readable { b"secret\r\n" } else { b"" };
    assert_eq!(stdout, expected, "{relative}");
}

/// The refusal `confine` gave, which must be one this host cannot establish.
fn unavailable(refused: Result<crate::process::Launch, Refusal>) -> String {
    match refused {
        Err(Refusal::Unavailable(why)) => why,
        Err(other) => panic!("expected an unavailable confinement, got {other}"),
        Ok(_) => panic!("expected a refusal, and confine let the launch through"),
    }
}

/// A Windows sandbox cannot have access taken away beneath a grant: the
/// `AppContainer` pass of the access check ignores a deny ACE naming a
/// capability. So the exclusion of `D` is carved out of the grant on `A`, and
/// every shape beneath `D` — inherited, protected with the grant copied down,
/// explicit — goes dark, while `A`'s other entries stay readable. An explicit
/// `ALL APPLICATION PACKAGES` entry beneath `D` is reach no carving removes, and
/// the launch is refused until it goes.
#[test]
fn a_deny_inside_a_grant_is_carved_out() {
    with_scoped_state(|| {
        let tree = Tree::new();
        let _session = SessionGuard;
        let a = tree.at("A");
        let (allow, deny) = (tree.real("A"), tree.real(r"A\D"));

        let whole = projection(&[&allow], &[]);
        assert_reads(&whole, &a, "ok.txt", true);
        tree.shape();
        for relative in READABLE {
            assert_reads(&whole, &a, relative, true);
        }
        assert_reads(&whole, &a, DARK, false);

        let carved = projection(&[&allow], &[&deny]);
        let why = unavailable(confined(&carved, &a, "type ok.txt"));
        assert!(
            why.contains("could still open") && why.contains("key.txt"),
            "{why}"
        );
        icacls(
            &tree.at(r"A\D\explicit\key.txt"),
            &["/remove", &format!("*{ALL_APPLICATION_PACKAGES}")],
        );
        assert_reads(&carved, &a, "ok.txt", true);
        for relative in READABLE.iter().skip(1).copied().chain([DARK]) {
            assert_reads(&carved, &a, relative, false);
        }
    });
}

/// The shape of exarch's `xdg-config` read less its credential directories:
/// the carved directory launches as the child's working directory, an
/// excluded entry is dark, its siblings readable, and neither an entry made
/// after the carving nor an excluded name that did not exist then and is made
/// after it is reached through the carved directory.
#[test]
fn an_xdg_config_style_grant_is_carved_and_launches() {
    with_scoped_state(|| {
        let tree = Tree::new();
        for relative in [r"cfg\gh\hosts.yml", r"cfg\git\config", r"cfg\other.txt"] {
            tree.write(relative);
        }
        let _session = SessionGuard;
        let cfg = tree.at("cfg");
        let (allow, gh) = (tree.real("cfg"), tree.real(r"cfg\gh"));
        let op = format!("{allow}/op");
        let carved = projection(&[&allow], &[&gh, &op]);

        assert_reads(&carved, &cfg, r"gh\hosts.yml", false);
        assert_reads(&carved, &cfg, r"git\config", true);
        assert_reads(&carved, &cfg, "other.txt", true);

        let launch = confined(&carved, &cfg, "type new.txt").unwrap();
        tree.write(r"cfg\new.txt");
        assert_launch_reads(launch, "new.txt", false);

        let launch = confined(&carved, &cfg, r"type op\session").unwrap();
        tree.write(r"cfg\op\session");
        assert_launch_reads(launch, r"op\session", false);
        assert_reads(&carved, &cfg, r"op\session", false);
    });
}

/// An exclusion no grant reaches launches, and stays out of reach.
#[test]
fn a_deny_apart_from_every_grant_launches_and_stays_dark() {
    with_scoped_state(|| {
        let tree = Tree::new();
        tree.write(r"B\key.txt");
        let _session = SessionGuard;
        let (allow, deny) = (tree.real("A"), tree.real("B"));
        let denied = projection(&[&allow], &[&deny]);
        assert_reads(&denied, &tree.at("A"), "ok.txt", true);
        assert_reads(&denied, &tree.at("A"), r"..\B\key.txt", false);
    });
}

/// What the lexical check cannot see, the access check does: a descendant
/// whose own DACL admits every sandbox, and a hard link carrying a granted
/// file's ACE to a name inside the exclusion. Either refuses the launch.
#[test]
fn a_deny_the_token_reaches_anyway_is_refused() {
    with_scoped_state(|| {
        let tree = Tree::new();
        let _session = SessionGuard;
        let a = tree.at("A");
        let allow = tree.real("A");
        // Stamp the grant on `A` first, so its ACE is on `ok.txt` to be linked.
        assert_reads(&projection(&[&allow], &[]), &a, "ok.txt", true);

        let open = tree.write(r"B\open\key.txt");
        tree.write(r"B\closed.txt");
        icacls(
            &open,
            &["/grant", &format!("*{ALL_APPLICATION_PACKAGES}:(RX)")],
        );
        let why = unavailable(confined(
            &projection(&[&allow], &[&tree.real("B")]),
            &a,
            "type ok.txt",
        ));
        assert!(why.contains("could still open"), "{why}");
        assert!(why.contains("key.txt"), "{why}");

        std::fs::create_dir(tree.at("C")).unwrap();
        std::fs::hard_link(tree.at(r"A\ok.txt"), tree.at(r"C\linked.txt")).unwrap();
        let why = unavailable(confined(
            &projection(&[&allow], &[&tree.real("C")]),
            &a,
            "type ok.txt",
        ));
        assert!(why.contains("linked.txt"), "{why}");
    });
}

/// A program's image is granted to the launch that runs it alone: a later
/// launch under the same projection does not read it.
#[test]
fn an_image_is_granted_to_its_own_launch_alone() {
    with_scoped_state(|| {
        let tree = Tree::new();
        let image = walked(tree.write(r"B\key.txt"));
        let _session = SessionGuard;
        let (a, only_a) = (tree.at("A"), projection(&[&tree.real("A")], &[]));
        let as_image = confined_as(&only_a, &a, r"type ..\B\key.txt", &image).unwrap();
        assert_launch_reads(as_image, r"..\B\key.txt", true);
        assert_reads(&only_a, &a, r"..\B\key.txt", false);
    });
}

/// A read-write folder every sandbox may already write still takes the `Low`
/// label its grant needs: without it the mandatory check refuses a create,
/// whatever the DACL admits.
#[test]
fn a_read_write_folder_every_sandbox_may_write_admits_a_create() {
    use crate::authority::sandbox::{ExecProjection, FsProjection, SandboxProjection};
    use crate::process::PgidPolicy;
    with_scoped_state(|| {
        let tree = Tree::new();
        let w = tree.at("W");
        std::fs::create_dir(&w).unwrap();
        icacls(
            &w,
            &["/grant", &format!("*{ALL_APPLICATION_PACKAGES}:(OI)(CI)F")],
        );
        let _session = SessionGuard;
        let allow = tree.real("W");
        let writable = SandboxProjection {
            fs: FsProjection::Restricted(rules(&[&allow], &[&allow], &[])),
            net: false,
            exec: ExecProjection::default(),
        };
        let mut launch = confined(&writable, &w, "echo ok>made.txt").unwrap();
        let (mut child, _pgid, _) = launch.spawn(PgidPolicy::Inherit).unwrap();
        let ended = child.reap().unwrap();
        assert!(
            w.join("made.txt").exists(),
            "the create was refused; the child ended {ended:?}"
        );
    });
}

#[test]
fn profile_name_is_pid_and_counter_keyed_within_the_length_cap() {
    let n0 = profile_name(0);
    let n1 = profile_name(1);
    assert_ne!(n0, n1);
    assert!(n0.starts_with(&format!("ral.sandbox.s{}.p", std::process::id())));
    // AppContainer profile names cap at 64 UTF-16 code units.
    assert!(profile_name(u32::MAX).encode_utf16().count() <= 64);
}
