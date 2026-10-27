use super::*;

fn file_rule(path: &RealPath, allow: bool) -> Rule {
    if allow {
        Rule::Allow(ExecAllow::support(path))
    } else {
        Rule::Deny(Box::new(ExecDeny::File(
            GrantPath::<Excluded>::walk(FrozenPath::of_real(path)).expect("a file walks"),
        )))
    }
}

fn veto_rule(name: &str) -> Rule {
    Rule::Deny(Box::new(ExecDeny::Name(crate::path::command_name_key(
        name,
    ))))
}

/// The command directories, plus whatever a test adds, by their real
/// paths as a grant freezes them, and the directory coreutils really live
/// in (`cat`'s), which a distribution may link `/usr/bin` into from a
/// multi-call image elsewhere, and the linkers a launch's support
/// supplies: nothing else is admitted, ral's own binary included.
fn admitting(dirs: &[&str], paths: &[&str]) -> ExecProjection {
    use crate::test_helper::real;
    let coreutils = real("/usr/bin/cat").as_path().parent().map(real);
    let dirs = ["/bin", "/usr/bin"]
        .iter()
        .chain(dirs)
        .map(real)
        .chain(coreutils)
        .map(|d| dir_rule(&d, true));
    let files = paths.iter().map(|p| file_rule(&real(p), true));
    let linkers = crate::authority::sandbox::carriers::linkers()
        .into_iter()
        .map(|linker| file_rule(&linker, true));
    ExecProjection::Restricted(dirs.chain(files).chain(linkers).collect())
}

/// Landlock's own precondition, alongside `envelope_launches`: without a
/// layer to enter there is nothing here to prove either way, so the test
/// stops; below ral's own minimum ABI it fails where the sandbox is required.
fn landlock_at_least(need: Abi) -> bool {
    crate::test_helper::confinement::landlock_reaches(need.level())
}

/// A runnable copy of `/bin/true` under `dir`, which no test admits.
fn planted_binary(dir: &std::path::Path) -> std::path::PathBuf {
    let copy = dir.join("planted");
    std::fs::copy("/bin/true", &copy).expect("copy /bin/true");
    copy
}

/// The interpreter bypass the in-process guard cannot see: `sh -c` re-execs
/// whatever it likes, and only the kernel is still looking.
#[test]
fn an_interpreter_cannot_exec_a_binary_outside_the_admits() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-bypass");
    let copy = planted_binary(&dir);
    let policy = exec_policy(admitting(&[], &[]));
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "'{copy}' && echo PLANTED-RAN\n\
         /bin/true && echo ADMITTED-RAN\n",
        copy = copy.display(),
    );
    let (stdout, _) = confined(&env, &policy, &script);
    assert!(
        !emitted(&stdout, "PLANTED-RAN"),
        "a binary outside every admit ran: {stdout}"
    );
    assert!(
        emitted(&stdout, "ADMITTED-RAN"),
        "the layer denied an admitted command, so the deny proves nothing: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The domain is inherited, so nesting interpreters buys nothing.
#[test]
fn a_nested_interpreter_inherits_the_layer() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-nested");
    let copy = planted_binary(&dir);
    let policy = exec_policy(admitting(&[], &[]));
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "sh -c '{copy}' && echo PLANTED-RAN\n\
         sh -c /bin/true && echo ADMITTED-RAN\n",
        copy = copy.display(),
    );
    let (stdout, _) = confined(&env, &policy, &script);
    assert!(
        !emitted(&stdout, "PLANTED-RAN"),
        "a second `sh -c` escaped the layer: {stdout}"
    );
    assert!(
        emitted(&stdout, "ADMITTED-RAN"),
        "the layer denied an admitted command two levels down: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// [`admitting`] less `blocks`, which Landlock renders by subtracting
/// them from the allowed directories that hold them.
fn admitting_less(dirs: &[&str], blocks: impl IntoIterator<Item = Rule>) -> ExecProjection {
    let ExecProjection::Restricted(rules) = admitting(dirs, &[]) else {
        unreachable!("admitting restricts");
    };
    ExecProjection::Restricted(rules.join(blocks.into_iter().collect()))
}

/// A command of `/usr/bin` with an inode of its own: a hard link or a
/// multi-call alias would share an inode the layer admits.
fn own_command(names: &[&str]) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    (names.iter())
        .map(|name| std::path::Path::new("/usr/bin").join(name))
        .find(|p| {
            p.symlink_metadata()
                .is_ok_and(|m| m.is_file() && m.nlink() == 1)
        })
}

/// A deny and a veto inside an allowed directory hold in the kernel, so
/// `sh -c`, which the guard never sees, reaches neither at any depth.
#[test]
fn an_exec_block_inside_an_allowed_directory_holds_for_grandchildren() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let (Some(denied), Some(vetoed)) = (
        own_command(&["id", "whoami", "tty"]),
        own_command(&["uname", "nproc", "logname"]),
    ) else {
        eprintln!("skipping: /usr/bin has no command with an inode of its own to block");
        return;
    };
    let name = vetoed.file_name().expect("a name").to_string_lossy();
    // A restricted fs that no write reaches `/usr/bin` through: a name
    // veto is walked only there.
    let work = workdir("exec-block");
    let policy = SandboxProjection {
        exec: admitting_less(
            &[],
            [
                file_rule(&crate::test_helper::real(&denied), false),
                veto_rule(&name),
            ],
        ),
        ..deny_within(&work, &[])
    };
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "sh -c '{denied}' >/dev/null 2>&1 && echo DENIED-RAN\n\
         sh -c \"sh -c '{denied}'\" >/dev/null 2>&1 && echo DENIED-NESTED-RAN\n\
         sh -c '{vetoed}' >/dev/null 2>&1 && echo VETOED-RAN\n\
         sh -c \"sh -c '{vetoed}'\" >/dev/null 2>&1 && echo VETOED-NESTED-RAN\n\
         sh -c /usr/bin/true && echo ADMITTED-RAN\n\
         sh -c 'sh -c /usr/bin/true' && echo ADMITTED-NESTED-RAN\n",
        denied = denied.display(),
        vetoed = vetoed.display(),
    );
    let (stdout, _) = confined(&env, &policy, &script);
    for ran in [
        "DENIED-RAN",
        "DENIED-NESTED-RAN",
        "VETOED-RAN",
        "VETOED-NESTED-RAN",
    ] {
        assert!(!emitted(&stdout, ran), "a blocked command ran: {stdout}");
    }
    for ran in ["ADMITTED-RAN", "ADMITTED-NESTED-RAN"] {
        assert!(
            emitted(&stdout, ran),
            "the layer denied an admitted sibling, so the blocks prove nothing: {stdout}"
        );
    }
}

/// An expanded directory is the tree as it stood at launch, so a program
/// added there is denied until the next launch; a directory with no block
/// beneath it is one hierarchy rule, which admits it at once.
#[test]
fn a_program_added_after_launch_runs_only_under_a_hierarchy_admit() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-added");
    let (expanded, whole) = (dir.join("expanded"), dir.join("whole"));
    for at in [&expanded, &whole] {
        std::fs::create_dir(at).expect("create an admitted dir");
    }
    let present = expanded.join("present");
    let blocked = expanded.join("blocked");
    for copy in [&present, &blocked] {
        std::fs::copy("/bin/true", copy).expect("copy /bin/true");
    }
    let policy = SandboxProjection {
        exec: admitting_less(
            &[&expanded.to_string_lossy(), &whole.to_string_lossy()],
            [file_rule(&crate::test_helper::real(&blocked), false)],
        ),
        ..restricted(&[], &[&expanded, &whole], &[])
    };
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "'{present}' && echo PRESENT-RAN\n\
         '{blocked}' && echo BLOCKED-RAN\n\
         cp /bin/true '{expanded}/added' && cp /bin/true '{whole}/added' || echo COPY-FAILED\n\
         '{expanded}/added' && echo EXPANDED-ADDED-RAN\n\
         '{whole}/added' && echo WHOLE-ADDED-RAN\n",
        present = present.display(),
        blocked = blocked.display(),
        expanded = expanded.display(),
        whole = whole.display(),
    );
    let (stdout, stderr) = confined(&env, &policy, &script);
    assert!(
        !emitted(&stdout, "COPY-FAILED"),
        "nothing was added, so nothing was tested: {stdout} {stderr}"
    );
    assert!(
        emitted(&stdout, "PRESENT-RAN"),
        "the expanded directory admitted nothing: {stdout}"
    );
    assert!(
        !emitted(&stdout, "BLOCKED-RAN"),
        "the block did not hold: {stdout}"
    );
    assert!(
        !emitted(&stdout, "EXPANDED-ADDED-RAN"),
        "a program added after launch ran in an expanded directory: {stdout}"
    );
    assert!(
        emitted(&stdout, "WHOLE-ADDED-RAN"),
        "a program added after launch did not run under a hierarchy admit: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the grant lets run it lets read, as on macOS: a run place outside
/// every fs prefix is shown, and without one it is not.
#[test]
fn a_run_place_is_readable_outside_every_fs_prefix() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-parity");
    let tools = dir.join("tools");
    std::fs::create_dir(&tools).unwrap();
    std::fs::write(tools.join("readme"), "PARITY-BYTES").unwrap();
    let elsewhere = workdir("exec-parity-elsewhere");
    let script = format!("cat '{}'\n", tools.join("readme").display());
    for (placed, exec) in [
        (true, admitting(&[&tools.to_string_lossy()], &[])),
        (false, admitting(&[], &[])),
    ] {
        let mut policy = SandboxProjection {
            exec,
            ..deny_within(&elsewhere, &[])
        };
        if let FsProjection::Restricted(rules) = &mut policy.fs {
            rules
                .read
                .extend(placed.then(|| FsAllow::RunPlace(crate::test_helper::real(&tools))));
        }
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let (stdout, _) = confined(&env, &policy, &script);
        assert_eq!(
            stdout.contains("PARITY-BYTES"),
            placed,
            "a run place is shown exactly when the read region lists it: {stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// A write into `dir/bin`, a write beside it in `dir`, and a run of the
/// tool already in `dir/bin`, each marked on stdout; a refusal carries
/// its reason.
fn authoring(dir: &std::path::Path) -> String {
    format!(
        "why=$(touch '{d}/bin/new' 2>&1) && echo ADMIT-WROTE || echo \"REFUSED: $why\"\n\
         touch '{d}/other' && echo PREFIX-WROTE\n\
         '{d}/bin/tool' && echo TOOL-RAN\n",
        d = dir.display()
    )
}

/// `authoring`'s output under `policy`, `None` where this host builds no
/// envelope.
fn authored(policy: &SandboxProjection, dir: &std::path::Path) -> Option<String> {
    let env = envelope_launches(policy)?;
    let (stdout, _) = confined(&env, policy, &authoring(dir));
    assert!(
        emitted(&stdout, "TOOL-RAN"),
        "the admit runs nothing: {stdout}"
    );
    Some(stdout)
}

/// Nothing is frozen: a write prefix that covers an admit without naming
/// it lets a child author what the admit runs, the declared limit of a
/// grant that writes where it runs, and takes the writes around it too.
#[test]
fn a_write_that_covers_an_admit_is_the_grants_to_give() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-unfrozen");
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::copy("/bin/true", bin.join("tool")).expect("copy /bin/true");
    let under = |write: &std::path::Path| SandboxProjection {
        exec: admitting(&[&bin.to_string_lossy()], &[]),
        ..deny_within(write, &[])
    };
    for write in [&dir, &bin] {
        let Some(wrote) = authored(&under(write), &dir) else {
            return;
        };
        assert!(
            emitted(&wrote, "ADMIT-WROTE"),
            "a write that reaches an admit was frozen out of it: {wrote}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_covered_file_is_as_writable_as_the_write_that_covers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let tool = dir.join("tool");
    for named in [false, true] {
        std::fs::copy("/bin/true", &tool).unwrap();
        let mut writes = vec![dir.as_path()];
        writes.extend(named.then_some(tool.as_path()));
        let policy = SandboxProjection {
            exec: admitting(&[], &[&tool.to_string_lossy()]),
            ..restricted(&[], &writes, &[])
        };
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY; echo x > '{t}' && echo REWRITTEN; echo y > '{d}/other' && echo SIBLING",
            t = tool.display(),
            d = dir.display()
        );
        let out = run_confined(&env, &policy, &script).unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("READY") && stdout.contains("SIBLING"),
            "{out:?}"
        );
        assert!(stdout.contains("REWRITTEN"), "named: {named}: {out:?}");
    }
}

/// Writes `dir/sibling` and `dir/script`, and returns the script's path.
/// The script prints its `$0`, then the sibling it finds by `${0%/*}`, the
/// lexical `dirname "$0"`, using builtins alone so that nothing else need
/// be admitted.
fn script_beside_sibling(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("script");
    let content = "#!/bin/sh\nprintf '%s\\n' \"$0\"; read -r l < \"${0%/*}/sibling\"; printf '%s\\n' \"$l\"\n";
    std::fs::write(&script, content).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("sibling"), "SIBLING\n").unwrap();
    script
}

/// The trampoline `execve`s the real path, so the kernel hands a `#!`
/// script that path as `$0`, and the lexical idiom finds its siblings.
#[test]
fn a_confined_script_sees_its_real_path_and_its_siblings() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let script = script_beside_sibling(&dir);
    let program = admitted(Program::file(script.clone()).unwrap());
    let policy = SandboxProjection {
        exec: admitting(&[], &[&script.to_string_lossy()]),
        ..restricted(&[&dir], &[], &[])
    };
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let out = run_admitted(&env, &policy, &program, &[]).unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\nSIBLING\n", script.display())
    );
}

/// A deny masks the file with an unopenable node, which the kernel's
/// `execve` refuses; `exec` names the real path.
#[test]
fn a_masked_script_does_not_run() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let script = script_beside_sibling(&dir);
    let program = admitted(Program::file(script.clone()).unwrap());
    let policy = SandboxProjection {
        exec: admitting(&[], &[&script.to_string_lossy()]),
        ..restricted(&[&dir], &[], &[&script])
    };
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let out = run_admitted(&env, &policy, &program, &[]).unwrap();
    assert!(!out.status.success() && out.stdout.is_empty(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&*script.to_string_lossy()),
        "{out:?}"
    );
}

/// Under an unrestricted fs, a veto in force or not, the tool the exec
/// grant admits still runs, and the child still writes beside it: every
/// write reaches the veto, so it is not walked.
#[test]
fn an_unrestricted_fs_with_a_veto_still_runs_what_the_exec_grant_admits() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-unrestricted-veto");
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::copy("/bin/true", bin.join("tool")).expect("copy /bin/true");
    for veto in [true, false] {
        let _ = std::fs::remove_file(bin.join("new"));
        let vetoes = veto.then(|| veto_rule("n"));
        let policy = exec_policy(admitting_less(&[&bin.to_string_lossy()], vetoes));
        let Some(stdout) = authored(&policy, &dir) else {
            return;
        };
        assert!(
            emitted(&stdout, "ADMIT-WROTE") && emitted(&stdout, "PREFIX-WROTE"),
            "veto: {veto}: {stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A runnable copy of `/bin/true` at `dir/name`, which it creates.
fn copy_true(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let copy = dir.join(name);
    std::fs::copy("/bin/true", &copy).expect("copy /bin/true");
    copy
}

/// `Any` under a restricted fs is a rule on every bind root, less the
/// exclusions inside each: a program in a bind runs, the excluded one
/// beside it does not, and an exclusion elsewhere changes neither.
#[test]
fn every_program_is_lowered_onto_each_bind_less_the_exclusions() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-any");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let bind = dir.join("w/deep/bind");
    let (prog, excluded) = (copy_true(&bind, "prog"), copy_true(&bind, "excluded"));
    let unrelated = copy_true(&dir.join("elsewhere"), "unrelated");
    let real = |p: &std::path::Path| crate::test_helper::real(p);
    let policy = SandboxProjection {
        exec: ExecProjection::Restricted(
            [
                Rule::Allow(ExecAllow::Any),
                file_rule(&real(&excluded), false),
                file_rule(&real(&unrelated), false),
            ]
            .into_iter()
            .collect(),
        ),
        ..restricted(&[&bind], &[], &[])
    };
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "'{prog}' && echo PROG-RAN\n\
         '{excluded}' && echo EXCLUDED-RAN\n",
        prog = prog.display(),
        excluded = excluded.display(),
    );
    let (stdout, stderr) = confined(&env, &policy, &script);
    assert!(
        emitted(&stdout, "PROG-RAN"),
        "Any admitted nothing in the bind: {stdout} {stderr}"
    );
    assert!(
        !emitted(&stdout, "EXCLUDED-RAN"),
        "an exclusion inside the bind did not hold: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory admission reaches binds whose roots lie inside it, which a
/// rule on the directory itself would never meet: an alias, a symlink to
/// a subdirectory that the envelope binds at both names, and a canonical
/// sub-bind under a grant that reads only that subdirectory.
#[test]
fn a_directory_admission_reaches_the_binds_inside_it() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-lowered");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (tools, sub) = (dir.join("tools"), dir.join("tools/sub"));
    copy_true(&sub, "prog");
    std::os::unix::fs::symlink("tools/sub", dir.join("alias")).unwrap();
    let exec = admitting(&[&tools.to_string_lossy()], &[]);
    for (label, read, run) in [
        ("alias", dir.join("alias"), dir.join("alias/prog")),
        ("sub-bind", sub.clone(), sub.join("prog")),
    ] {
        let policy = SandboxProjection {
            exec: exec.clone(),
            ..restricted(&[&read], &[], &[])
        };
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!("'{}' && echo PROG-RAN\n", run.display());
        let (stdout, stderr) = confined(&env, &policy, &script);
        assert!(
            emitted(&stdout, "PROG-RAN"),
            "{label}: the admission missed the bind: {stdout} {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A layer about exec must leave the two ordinary shapes alone: a dynamic
/// binary, whose loader needs `Execute` of its own, and a `#!` script,
/// which needs it on both the script and its interpreter.
#[test]
fn an_admitted_dynamic_binary_and_shebang_script_both_run() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = workdir("exec-shebang");
    let script_file = dir.join("hello.sh");
    std::fs::write(&script_file, "#!/bin/sh\necho SCRIPT-RAN\n").expect("write script");
    std::fs::set_permissions(&script_file, std::fs::Permissions::from_mode(0o755))
        .expect("make the script runnable");
    let policy = exec_policy(admitting(&[&dir.to_string_lossy()], &[]));
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "cat /dev/null && echo DYNAMIC-RAN\n\
         '{file}'\n",
        file = script_file.display(),
    );
    let (stdout, stderr) = confined(&env, &policy, &script);
    for ran in ["DYNAMIC-RAN", "SCRIPT-RAN"] {
        assert!(stdout.contains(ran), "missing {ran}: {stdout} {stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A domain that does not *handle* `Refer` refuses every cross-directory
/// rename with `EXDEV`, which a layer about exec must not do.  Only a
/// spawn sees it, and only the inode does: `mv` turns that refusal into a
/// copy and an unlink.
#[test]
fn a_cross_directory_rename_inside_the_write_prefix_still_works() {
    if !landlock_at_least(Abi::REFER) {
        return;
    }
    let dir = workdir("exec-refer");
    let (from, to) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(&from).unwrap();
    std::fs::create_dir_all(&to).unwrap();
    std::fs::write(from.join("x"), "MOVED-BYTES").unwrap();
    let policy = exec_policy(admitting(&[], &[]));
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "echo READY\n\
         set -- $(ls -i '{from}/x'); echo INODE-BEFORE=$1\n\
         mv '{from}/x' '{to}/x' && echo MOVED\n\
         set -- $(ls -i '{to}/x'); echo INODE-AFTER=$1\n",
        from = from.display(),
        to = to.display(),
    );
    let out = run_confined(&env, &policy, &script).expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        emitted(&stdout, "MOVED"),
        "the exec layer refused a rename it has no business refusing: {stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(to.join("x").exists(), "the rename did not land on the host");
    let before = field(&stdout, "INODE-BEFORE=").expect("an inode before the move");
    assert_eq!(
        field(&stdout, "INODE-AFTER="),
        Some(before),
        "`mv` fell back to copy-and-unlink, so the rename itself was refused: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/tmp` is a confined child's own writable ground on every projection,
/// so nothing but the exec layer stops it dropping a binary there and
/// running it.  Both halves are asserted: an admit that changes nothing
/// would read as a pass against half of this.
#[test]
fn a_binary_written_to_tmp_runs_only_where_tmp_is_admitted() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-tmp");
    let planted = dir.join("planted");
    let script = format!(
        "cp /bin/true '{planted}' || echo COPY-FAILED\n\
         '{planted}' && echo TMP-RAN\n",
        planted = planted.display(),
    );
    let script = script.as_str();
    for (admits_tmp, dirs) in [(false, &[][..]), (true, &["/tmp"][..])] {
        let policy = exec_policy(admitting(dirs, &[]));
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let (stdout, stderr) = confined(&env, &policy, script);
        assert!(
            !emitted(&stdout, "COPY-FAILED"),
            "the drop into /tmp did not happen, so nothing was tested: {stdout} {stderr}"
        );
        assert_eq!(
            emitted(&stdout, "TMP-RAN"),
            admits_tmp,
            "/tmp exec must follow the admits alone: {stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The scope rides every launch, whatever the exec axis says: without it a
/// confined child signals same-uid host processes wherever the host cannot
/// build a pid namespace.  Which errno the refusal carries is the kernel's
/// business — `ESRCH` behind a pid namespace, `EPERM` without — so only
/// the refusal is asserted.
#[test]
fn a_confined_child_cannot_signal_a_host_process() {
    if !landlock_at_least(Abi::SCOPES) {
        return;
    }
    let dir = workdir("exec-scope");
    let mut sleeper = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn a host-side sleeper");
    let script = format!(
        "kill -0 {pid} 2>/dev/null && echo SLEEPER-VISIBLE\n\
         kill -TERM {pid} 2>/dev/null && echo SLEEPER-SIGNALLED\n",
        pid = sleeper.id(),
    );
    let bare = ExecProjection::Restricted(
        [Rule::Allow(ExecAllow::Any), veto_rule("rm")]
            .into_iter()
            .collect(),
    );
    for exec in [ExecProjection::Unrestricted, admitting(&[], &[]), bare] {
        let policy = exec_policy(exec);
        let Some(env) = envelope_launches(&policy) else {
            break;
        };
        let (stdout, _) = confined(&env, &policy, &script);
        assert!(
            !emitted(&stdout, "SLEEPER-VISIBLE") && !emitted(&stdout, "SLEEPER-SIGNALLED"),
            "a host process was reachable from inside the envelope: {stdout}"
        );
        assert!(
            sleeper.try_wait().expect("poll the sleeper").is_none(),
            "the host-side sleeper was killed from inside the envelope"
        );
    }
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Nothing is tightened that the projection did not ask for.
#[test]
fn an_unrestricted_exec_projection_leaves_every_binary_runnable() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-open");
    let copy = planted_binary(&dir);
    let policy = exec_policy(ExecProjection::Unrestricted);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "echo READY\n'{copy}' && echo PLANTED-RAN\n",
        copy = copy.display()
    );
    let out = run_confined(&env, &policy, &script).expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        emitted(&stdout, "PLANTED-RAN"),
        "an unrestricted exec projection must confine no exec at all: {stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ral's binary is an ordinary file: it runs where the table admits it,
/// by path or by directory, and nowhere else, a byte-identical copy
/// beside it included.
#[test]
fn ral_runs_only_where_the_table_admits_it() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
    let dir = workdir("exec-self");
    let copy = dir.join("ral-copy");
    std::fs::copy(own.arg0(), &copy).expect("copy ral");
    let own_path = own.arg0().to_string_lossy().into_owned();
    let script = format!(
        "'{own}' --list >/dev/null && echo SELF-RAN\n\
         '{copy}' --list >/dev/null && echo COPY-RAN\n",
        own = own.arg0().display(),
        copy = copy.display(),
    );
    for (label, exec, self_ran) in [
        ("unnamed", admitting(&[], &[]), false),
        ("by path", admitting(&[], &[&own_path]), true),
    ] {
        let policy = exec_policy(exec);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let (stdout, _) = confined(&env, &policy, &script);
        assert_eq!(emitted(&stdout, "SELF-RAN"), self_ran, "{label}: {stdout}");
        assert!(
            !emitted(&stdout, "COPY-RAN"),
            "{label}: a copy of ral ran, so the admit is not its file: {stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The stack as a launch folds it: exec `allow`ed and `deny`ed by path,
/// each frozen, a directory when it is one, beside the system's command
/// directories; `fs` as given.  The program is admitted under it.
fn folded(
    exec: Option<(&[&std::path::Path], &[&std::path::Path])>,
    fs: Option<FsPolicy>,
    program: &std::path::Path,
) -> (SandboxProjection, Admitted) {
    use crate::authority::policy::{ExecGrant, ExecKey};
    let key = |p: &std::path::Path| {
        let frozen = FrozenPath::from_surface(crate::path::utf8(p).unwrap());
        if p.is_dir() {
            ExecKey::Dir(frozen)
        } else {
            ExecKey::Path(frozen)
        }
    };
    let exec = exec.map(|(allow, deny)| {
        ExecGrant::of(
            ["/bin", "/usr/bin"]
                .iter()
                .map(|d| key(d.as_ref()))
                .chain(allow.iter().map(|p| key(p))),
            deny.iter().map(|p| key(p)),
        )
    });
    let stack = crate::test_helper::stack([crate::test_helper::layer(Capabilities {
        exec,
        fs,
        ..Capabilities::default()
    })]);
    let baseline = host_system_reads();
    let program = Program::file(program.to_path_buf()).expect("the program exists");
    let admitted = stack
        .admit(&baseline, program)
        .unwrap_or_else(|why| panic!("the stack refuses the program: {why:?}"));
    let projection = SandboxProjection::of(&stack, &baseline, Some(&admitted))
        .expect("a restricted stack projects");
    (projection, admitted)
}

fn exec_files(projection: &SandboxProjection) -> Vec<RealPath> {
    match &projection.exec {
        ExecProjection::Restricted(rules) => rules.allowed_files().cloned().collect(),
        ExecProjection::Unrestricted => Vec::new(),
    }
}

/// An exec exclusion on a linker candidate beats the support that would
/// admit it; the same grant without it admits the linker.
#[test]
fn an_excluded_linker_candidate_is_not_admitted() {
    let Some(linker) = crate::authority::sandbox::carriers::linkers()
        .into_iter()
        .next()
    else {
        eprintln!("skipping: this host has no dynamic linker candidate");
        return;
    };
    let sh = std::path::Path::new("/bin/sh");
    let (open, _) = folded(Some((&[], &[])), None, sh);
    assert!(
        exec_files(&open).contains(&linker),
        "support admits the linker unless excluded: {linker}"
    );
    let (shut, _) = folded(Some((&[], &[linker.as_path()])), None, sh);
    assert!(
        !exec_files(&shut).contains(&linker),
        "an excluded linker candidate was admitted: {linker}"
    );
}

/// An fs exclusion on a linker candidate beats the support that would
/// admit it, as an exec exclusion does.
#[test]
fn an_fs_excluded_linker_candidate_is_not_admitted() {
    let Some(linker) = crate::authority::sandbox::carriers::linkers()
        .into_iter()
        .next()
    else {
        eprintln!("skipping: this host has no dynamic linker candidate");
        return;
    };
    let sh = std::path::Path::new("/bin/sh");
    let fs = |deny: Vec<FrozenPath>| FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(
            crate::path::utf8(std::path::Path::new("/")).unwrap(),
        )],
        write_prefixes: Vec::new(),
        deny_paths: deny,
    };
    let (open, _) = folded(Some((&[], &[])), Some(fs(Vec::new())), sh);
    assert!(
        exec_files(&open).contains(&linker),
        "support admits the linker unless excluded: {linker}"
    );
    let denied = fs(vec![FrozenPath::from_surface(
        crate::path::utf8(linker.as_path()).unwrap(),
    )]);
    let (shut, _) = folded(Some((&[], &[])), Some(denied), sh);
    assert!(
        !exec_files(&shut).contains(&linker),
        "an fs-excluded linker candidate was admitted: {linker}"
    );
}

/// An exec exclusion on a script's interpreter keeps it out of the read
/// region's places, where support would otherwise carry it.
#[test]
fn an_exec_excluded_carrier_is_not_read() {
    let Some((script, carrier)) = crate::authority::sandbox::carriers::system_script() else {
        eprintln!("skipping: this host has no system script");
        return;
    };
    let reads = || FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(
            crate::path::utf8(&std::env::temp_dir()).unwrap(),
        )],
        write_prefixes: Vec::new(),
        deny_paths: Vec::new(),
    };
    let carried = |deny: &[&std::path::Path]| {
        let (policy, _) = folded(Some((&[], deny)), Some(reads()), script.as_path());
        let rules = policy.fs.rules().expect("a restricted fs").read.clone();
        rules.contains(&FsAllow::RunPlace(carrier.clone()))
    };
    assert!(carried(&[]), "support carries the interpreter: {carrier}");
    assert!(
        !carried(&[carrier.as_path()]),
        "an exec-excluded carrier was read: {carrier}"
    );
}

/// A `#!` script at `dir/script` whose interpreter, a copy of the shell,
/// is `dir/interp`.
fn scripted(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let interp = dir.join("interp");
    std::fs::copy("/bin/sh", &interp).expect("copy /bin/sh");
    let script = dir.join("script");
    std::fs::write(
        &script,
        format!("#!{}\necho INTERP-RAN\n", interp.display()),
    )
    .expect("write the script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make the script runnable");
    (script, interp)
}

/// A script whose interpreter is excluded, by exec or by fs, fails in the
/// kernel, and the hint names the exclusion rather than a missing admit.
#[test]
fn a_script_whose_interpreter_is_excluded_fails_in_the_kernel_naming_it() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-interp-excluded");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (script, interp) = scripted(&dir);
    let fs = FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&dir).unwrap())],
        write_prefixes: Vec::new(),
        deny_paths: vec![FrozenPath::from_surface(
            crate::path::utf8(&interp).unwrap(),
        )],
    };
    for (axis, exec, fs) in [
        (
            "exec",
            Some((&[dir.as_path()][..], &[interp.as_path()][..])),
            None,
        ),
        ("fs", Some((&[dir.as_path()][..], &[][..])), Some(fs)),
    ] {
        let (policy, admitted) = folded(exec, fs, &script);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_admitted(&env, &policy, &admitted, &[]).expect("spawn");
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            !stdout.contains("INTERP-RAN") && !out.status.success(),
            "{axis}: an excluded interpreter ran: {stdout} {stderr}"
        );
        assert!(
            stderr.contains("interpreter") && stderr.contains("is excluded by"),
            "{axis}: the hint does not name the exclusion: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A trusted script starts under a restricted fs whose reads name neither
/// it nor its interpreter, with exec supplied and omitted.
#[test]
fn a_trusted_script_starts_through_live_supports_reads() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let Some((script, _)) = crate::authority::sandbox::carriers::system_script() else {
        eprintln!("skipping: this host has no system script");
        return;
    };
    let dir = workdir("trusted-script");
    let reads = || FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&dir).unwrap())],
        write_prefixes: Vec::new(),
        deny_paths: Vec::new(),
    };
    // The trampoline's refusal of the launch, the script's or its
    // interpreter's, unread or unadmitted; the script's children are its own.
    let refused = format!("ral: {script}:");
    for (label, exec) in [
        ("exec supplied", Some((&[script.as_path()][..], &[][..]))),
        ("exec omitted", None),
    ] {
        let (policy, admitted) = folded(exec, Some(reads()), script.as_path());
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_admitted(&env, &policy, &admitted, &["--help".into()]).expect("spawn");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains(&refused), "{label}: {stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The exec policies a descendant meets ral under, each with whether ral
/// starts: admitted by path, not admitted, its folder excluded.
fn ral_exec_policies(work: &std::path::Path) -> [(&'static str, SandboxProjection, bool); 3] {
    let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
    let home = own.arg0().parent().expect("ral has a folder");
    let own_path = own.arg0().to_string_lossy().into_owned();
    let under = |exec, deny: &[&std::path::Path]| SandboxProjection {
        exec,
        ..deny_within(work, deny)
    };
    [
        ("admitted", under(admitting(&[], &[&own_path]), &[]), true),
        ("unadmitted", under(admitting(&[], &[]), &[]), false),
        (
            "excluded folder",
            under(admitting(&[&home.to_string_lossy()], &[]), &[home]),
            false,
        ),
    ]
}

/// A descendant starts ral neither under an exec allow-list that does not
/// admit it, nor by name where its folder is fs-excluded, nor as
/// `/proc/self/exe`.
#[test]
fn a_descendant_cannot_start_ral() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
    let home = own.arg0().parent().expect("ral has a folder");
    let name = own.arg0().file_name().expect("ral has a name");
    let work = workdir("exec-ral");
    let script = format!(
        "PATH='{home}' '{name}' --list >/dev/null && echo SELF-RAN\n\
         env /proc/self/exe -c true && echo PROC-RAN\n",
        home = home.display(),
        name = name.to_string_lossy(),
    );
    for (label, policy, self_ran) in ral_exec_policies(&work) {
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let (stdout, _) = confined(&env, &policy, &script);
        assert_eq!(emitted(&stdout, "SELF-RAN"), self_ran, "{label}: {stdout}");
        assert!(!emitted(&stdout, "PROC-RAN"), "{label}: {stdout}");
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// Under the same policies a bundled tool, run by the launcher, still runs.
#[cfg(feature = "coreutils")]
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn a_bundled_tool_runs_where_a_descendant_cannot_start_ral() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let work = workdir("exec-ral-tool");
    let tool = admitted(Program::Tool("basename"));
    for (label, policy, _) in ral_exec_policies(&work) {
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_admitted(&env, &policy, &tool, &["/x/TOOL-RAN".to_string()]).expect("spawn");
        assert!(
            emitted(&String::from_utf8_lossy(&out.stdout), "TOOL-RAN"),
            "{label}: a bundled tool did not run: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// A hard link made before launch at an excluded path to an admitted file
/// runs: Landlock admits inodes, and the exclusion names a path (§7.6).
#[test]
fn a_preexisting_hard_link_at_an_excluded_path_runs() {
    if !landlock_at_least(Abi::EXEC) {
        return;
    }
    let dir = workdir("exec-hard-link");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let admitted_file = copy_true(&dir, "admitted");
    let linked = dir.join("linked");
    std::fs::hard_link(&admitted_file, &linked).expect("hard link");
    let planted = copy_true(&dir, "planted");
    let policy = exec_policy(admitting_less(
        &[],
        [
            file_rule(&crate::test_helper::real(&admitted_file), true),
            file_rule(&crate::test_helper::real(&linked), false),
        ],
    ));
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "'{linked}' && echo LINK-RAN\n\
         '{planted}' && echo PLANTED-RAN\n",
        linked = linked.display(),
        planted = planted.display(),
    );
    let (stdout, _) = confined(&env, &policy, &script);
    assert!(
        emitted(&stdout, "LINK-RAN"),
        "the declared limit moved: {stdout}"
    );
    assert!(
        !emitted(&stdout, "PLANTED-RAN"),
        "an unadmitted copy ran, so the link proves nothing: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Refused while building, before bwrap is ever spawned: the stand-in
/// envelope would run this test binary if it were.
#[test]
fn an_exec_restricting_launch_without_landlock_spawns_nothing() {
    let host = HostEnvelope {
        landlock: Landlock::Absent,
        ..WHOLE
    };
    let sh = Program::file("/bin/sh".into()).expect("/bin/sh exists");
    let launch = enveloped(
        &on(&stand_in(), host),
        &exec_policy(admitting(&[], &[])),
        &Baseline::default(),
        &admitted(sh),
        &["-c".to_string(), "echo RAN".to_string()],
        None,
        Ownership::Kept,
    );
    let Err(e) = launch else {
        panic!("a restricted exec grant needs Landlock");
    };
    assert!(e.to_string().contains("Landlock"), "{e}");
}

/// A warrant promising a ruleset is the trampoline's whole word on it, so
/// one that never arrived — at a slot the launch held but never handed
/// down — is a launch it refuses, not one it runs open.
#[test]
fn a_promised_ruleset_that_never_arrived_runs_nothing() {
    use crate::authority::sandbox::warrant::Warrant;
    use std::os::fd::AsFd;
    let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
    let sh = Program::file("/bin/sh".into()).expect("/bin/sh exists");
    let null = std::fs::File::open("/dev/null").expect("open /dev/null");
    let mut withheld = Handoff::default();
    let at = withheld.lend(null.as_fd()).expect("lends");
    let promise = Some(Landlocked {
        refer_root: false,
        at,
    });
    let parcel = Warrant::new(
        promise,
        &admitted(sh),
        &["-c".to_string(), "echo RAN".to_string()],
        &unrestricted(),
    )
    .parcel()
    .expect("parcels");
    let mut handoff = Handoff::default();
    let warrant = handoff.lend(parcel.as_fd()).expect("lends");
    let mut cmd = own.command();
    cmd.arg(Role::Warrant.flag())
        .arg(warrant.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    handoff.install(&mut cmd);
    let out = cmd.output().expect("spawn the trampoline");
    drop(withheld);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(126), "{stdout} {stderr}");
    assert!(!emitted(&stdout, "RAN"), "the program ran unconfined");
    assert!(stderr.contains(&format!("fd {at}")), "{stderr}");
}
