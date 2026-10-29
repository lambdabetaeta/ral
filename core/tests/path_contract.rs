#![allow(clippy::disallowed_methods, reason = "test code")]

//! The path contracts at the shell boundary: an access is resolved as the
//! kernel resolves it, a grant prefix is a logical spelling, and the root
//! and a name ending in a separator are directories.

mod common;

use ral_core::Value;
use ral_core::protocol::Run;
use ral_core::shell::runtime::Settled;
use ral_core::source::SourceOrigin;
use std::path::{Path, PathBuf};

/// `body` run with `dir` for its working directory.
fn run_in(dir: &Path, body: &str) -> Settled<Value> {
    let source = format!("within [dir: '{}'] {{ {body} }}", dir.display());
    common::fresh_shell()
        .run(Run::captured(source, SourceOrigin::named("<test>")))
        .expect_ran()
        .into_result()
}

fn shown(dir: &Path, body: &str) -> String {
    run_in(dir, body)
        .unwrap_or_else(|_| panic!("{body} must run"))
        .to_string()
}

/// A tempdir as the walk resolves it, holding `file` and `deep/dir`.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    std::fs::create_dir_all(dir.join("deep/dir")).unwrap();
    std::fs::write(dir.join("file"), "original").unwrap();
    (tmp, dir)
}

/// A deny on a dangling link names its target, as the write through it does.
#[cfg(unix)]
#[test]
fn a_deny_on_a_dangling_link_holds_at_its_target() {
    let (_tmp, dir) = fixture();
    std::os::unix::fs::symlink("absent", dir.join("dlink")).unwrap();
    for target in ["dlink", "absent"] {
        let body = format!(
            "grant [fs: [read: [`absolute /], write: [`cwd .], deny: [`cwd dlink]]] {{ echo pwn > {target} }}"
        );
        assert!(run_in(&dir, &body).is_err(), "> {target} must be refused");
    }
    assert!(!dir.join("absent").exists());
}

/// A trailing separator names a directory, and the kernel's ENOTDIR stands.
#[cfg(unix)]
#[test]
fn a_trailing_separator_names_a_directory() {
    let (_tmp, dir) = fixture();
    std::os::unix::fs::symlink("file/", dir.join("slashlink")).unwrap();
    assert_eq!(shown(&dir, "is-file 'file/'"), "false");
    assert_eq!(shown(&dir, "is-dir 'deep/'"), "true");
    assert!(run_in(&dir, "echo x > 'file/'").is_err());
    assert!(run_in(&dir, "echo x > slashlink").is_err());
    assert_eq!(
        std::fs::read_to_string(dir.join("file")).unwrap(),
        "original"
    );
}

/// `resolve-path` reads `..` physically; `absolute-path` folds it.
#[cfg(unix)]
#[test]
fn resolve_path_is_physical_and_absolute_path_is_lexical() {
    let (_tmp, dir) = fixture();
    std::fs::write(dir.join("deep/file"), "").unwrap();
    std::os::unix::fs::symlink("deep/dir", dir.join("link")).unwrap();
    let at = |rel: &str| dir.join(rel).display().to_string();
    assert_eq!(shown(&dir, "resolve-path 'link/../file'"), at("deep/file"));
    assert!(run_in(&dir, "resolve-path 'missing/../file'").is_err());
    for raw in ["link/../file", "missing/../file"] {
        assert_eq!(shown(&dir, &format!("absolute-path '{raw}'")), at("file"));
    }
}

#[test]
fn the_root_is_a_directory() {
    let (_tmp, dir) = fixture();
    assert_eq!(shown(&dir, "exists '/'"), "true");
    assert_eq!(shown(&dir, "is-dir '/'"), "true");
    assert_eq!(shown(&dir, "$[!{length !{list-dir '/'}} > 0]"), "true");
    assert_eq!(shown(&dir, "!{file-info '/'}[type]"), "`dir");
    assert!(run_in(&dir, "echo x > '/'").is_err());
}

/// A deny on `dlink` holds the link and the target it had; retargeted inside
/// the block by an external `ln` at the file `moved` (named by `printf`), it
/// holds `dlink` itself and the old target, and not `moved`.
#[cfg(target_os = "linux")]
fn deny_holds_what_it_held_when_its_link_is_retargeted(moved: &[u8], printf: &str) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, dir) = fixture();
    let moved = dir.join(OsStr::from_bytes(moved));
    std::fs::write(&moved, "original").unwrap();
    std::fs::write(dir.join("tgt"), "original").unwrap();
    let script = dir.join("retarget.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nln -sfn \"$(printf '{printf}')\" dlink\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let after_retarget = |op: &str| {
        std::fs::remove_file(dir.join("dlink")).ok();
        std::os::unix::fs::symlink("tgt", dir.join("dlink")).unwrap();
        let body = format!(
            "grant [fs: [read: [`absolute /], write: [`cwd .], deny: [`cwd dlink]]] \
             {{ ./retarget.sh; {op} }}"
        );
        run_in(&dir, &body)
    };
    assert!(
        after_retarget("exists dlink").is_err(),
        "the link itself stays denied"
    );
    assert!(
        after_retarget("to-string overwritten > 'tgt'").is_err(),
        "the old target stays denied"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tgt")).unwrap(),
        "original"
    );
    assert!(
        after_retarget("to-string overwritten > 'dlink'").is_ok(),
        "what the link now reaches was never denied"
    );
    assert_eq!(std::fs::read_to_string(&moved).unwrap(), "overwritten");
}

#[cfg(target_os = "linux")]
#[test]
fn a_deny_holds_a_retargeted_link_and_its_old_target_not_the_new_one() {
    deny_holds_what_it_held_when_its_link_is_retargeted(b"tgt2", "tgt2");
}

#[cfg(target_os = "linux")]
#[test]
fn a_deny_holds_a_link_retargeted_at_a_name_that_is_not_unicode_likewise() {
    deny_holds_what_it_held_when_its_link_is_retargeted(b"tgt\xFF", "tgt\\377");
}

/// `xdg:` reads the `XDG_*_HOME` of the effective environment, an overlay
/// included.
#[cfg(unix)]
#[test]
fn xdg_reads_the_overlay_environment() {
    let (_tmp, dir) = fixture();
    let body = format!(
        "within [env: [XDG_CONFIG_HOME: '{}']] {{ absolute-path 'xdg:config/probe' }}",
        dir.display()
    );
    assert_eq!(shown(&dir, &body), format!("{}/probe", dir.display()));
}

/// `..` climbs before the first wildcard, as written, and is refused after it.
#[cfg(unix)]
#[test]
fn glob_climbs_only_before_a_wildcard() {
    let (_tmp, dir) = fixture();
    std::fs::write(dir.join("deep/b.rs"), "").unwrap();
    let Err(e) = run_in(&dir, "glob '*/../*.rs'") else {
        panic!("glob '*/../*.rs' must be refused");
    };
    assert!(e.to_string().contains("'..'"), "{e}");
    assert_eq!(shown(&dir.join("deep/dir"), "glob '../*.rs'"), "[../b.rs]");
}

/// A deny on a name that is not there refuses the question and the read alike,
/// so a denied absent name never reads as merely absent: the grant judges the
/// create-target object the walk finds, and where an intermediate directory is
/// absent too, `exists` and `file-info` judge the name itself.
#[cfg(unix)]
#[test]
fn a_denied_absent_name_is_refused_not_reported_absent() {
    let (_tmp, dir) = fixture();
    let refused = |deny: &str, body: &str| {
        let body = format!("grant [fs: [read: [`absolute /], deny: [`cwd {deny}]]] {{ {body} }}");
        let message = run_in(&dir, &body).expect_err("a denied name must be refused");
        let message = format!("{message:?}");
        assert!(
            message.contains("denied by grant"),
            "{body} must be refused by the grant, got: {message}"
        );
    };
    for body in ["exists deep/.env", "file-info deep/.env", "cat < deep/.env"] {
        refused("deep/.env", body);
    }
    for body in ["exists gone/x", "file-info gone/x"] {
        refused("gone", body);
    }
}

/// A walk that fails under a denied tree, at a missing component or a file
/// where a directory is due, is refused by the grant, in a write redirect as
/// in a read: the error never says what is there.  Under an allowance it is
/// still the walk's.
#[cfg(unix)]
#[test]
fn a_walk_failing_under_a_denied_tree_is_refused_by_the_grant() {
    let (_tmp, dir) = fixture();
    std::fs::create_dir_all(dir.join("allowed")).unwrap();
    std::fs::create_dir_all(dir.join("denied/sub")).unwrap();
    std::fs::write(dir.join("denied/sub/f"), "").unwrap();
    let run = |body: &str| {
        let body =
            format!("grant [fs: [read: [`cwd allowed], write: [`cwd allowed]]] {{ {body} }}");
        let err = run_in(&dir, &body).expect_err("the walk must fail");
        format!("{err:?}")
    };
    for body in [
        "from-string < denied/sub/f",
        "from-string < denied/sub/nosuch",
        "from-string < denied/nosuch/f",
        "from-string < denied/sub/f/x",
        "exists denied/nosuch/f",
        "echo x > denied/nosuch/f",
        "echo x > denied/sub/f/x",
    ] {
        assert_fs_denial(
            &dir,
            &format!("grant [fs: [read: [`cwd allowed], write: [`cwd allowed]]] {{ {body} }}"),
        );
    }
    let message = run("from-string < allowed/nosuch/f");
    assert!(message.contains("no such file or directory"), "{message}");
}

#[cfg(unix)]
fn assert_fs_denial(dir: &Path, body: &str) {
    use ral_core::data::{FOValue, datum::Datum};
    use ral_core::fact::{Decision, Observation, Observed, Resource};

    let report = run_in(dir, &format!("audit {{ {body} }}")).expect("audit returns the refusal");
    let Value::Map(report) = report else {
        panic!("audit must return a record");
    };
    let outcome = report.get("outcome").unwrap().to_string();
    assert!(outcome.contains("denied by grant"), "{body}: {outcome}");
    assert!(!outcome.contains("too many levels"), "{body}: {outcome}");
    let trail = report.get("trail").unwrap();
    let Value::List(trail) = trail.as_ref() else {
        panic!("audit must return a trail");
    };
    let checks: Vec<_> = trail
        .iter()
        .filter_map(|fact| {
            let fact = FOValue::try_from(fact.as_ref()).unwrap();
            match Observation::decode(&fact).unwrap().what {
                Observed::Check(check) => Some(check),
                _ => None,
            }
        })
        .collect();
    assert_eq!(checks.len(), 1, "{body}: {trail:?}");
    assert_eq!(checks[0].resource, Resource::Fs);
    assert_eq!(checks[0].decision(), Decision::Denied);
}

#[cfg(unix)]
#[test]
fn a_loop_diagnostic_is_judged_at_the_failed_link() {
    use std::os::unix::fs::symlink;

    let (_tmp, dir) = fixture();
    std::fs::create_dir(dir.join("allowed")).unwrap();
    std::fs::create_dir(dir.join("denied")).unwrap();
    symlink("relative", dir.join("denied/relative")).unwrap();
    symlink(dir.join("denied/absolute"), dir.join("denied/absolute")).unwrap();
    symlink("loop", dir.join("allowed/loop")).unwrap();
    symlink("../denied/relative", dir.join("allowed/out")).unwrap();
    symlink("../allowed/loop", dir.join("denied/in")).unwrap();
    std::fs::write(dir.join("allowed/ok"), "allowed").unwrap();
    symlink("../allowed/ok", dir.join("denied/link")).unwrap();
    symlink("../denied/relative", dir.join("allowed/bad-link")).unwrap();

    for policy in [
        "read: [`cwd allowed], write: [`cwd allowed]",
        "read: [`cwd .], write: [`cwd .], deny: [`cwd denied]",
        "read: [`cwd allowed/ok], write: [`cwd allowed/ok]",
    ] {
        for path in ["denied/relative", "denied/absolute", "allowed/out"] {
            for op in ["exists", "file-info", "from-string <", "echo x >"] {
                let body = format!("grant [fs: [{policy}]] {{ {op} {path}/child }}");
                assert_fs_denial(&dir, &body);
            }
        }
    }
    for path in ["allowed/loop", "denied/in"] {
        for op in ["exists", "file-info", "from-string <", "echo x >"] {
            let body = format!(
                "grant [fs: [read: [`cwd allowed], write: [`cwd allowed]]] {{ {op} {path}/child }}"
            );
            let error = run_in(&dir, &body).expect_err("an admitted loop must remain an error");
            assert!(
                error.to_string().contains("too many levels"),
                "{body}: {error}"
            );
        }
    }
    assert_eq!(
        shown(
            &dir,
            "grant [fs: [read: [`cwd allowed], deny: [`cwd denied]]] { from-string < denied/link }"
        ),
        "allowed"
    );
    assert_fs_denial(
        &dir,
        "grant [fs: [read: [`cwd allowed]]] { from-string < allowed/bad-link }",
    );
}

#[cfg(unix)]
#[test]
fn a_refused_link_does_not_disclose_its_target_in_errors_or_audit() {
    use std::os::unix::fs::symlink;

    let (_tmp, dir) = fixture();
    std::fs::create_dir(dir.join("hidden")).unwrap();
    std::fs::write(dir.join("hidden/undisclosed-file"), "secret").unwrap();
    std::fs::create_dir(dir.join("hidden/undisclosed-directory")).unwrap();
    symlink("undisclosed-file", dir.join("hidden/file")).unwrap();
    symlink(
        dir.join("hidden/undisclosed-file"),
        dir.join("hidden/absolute"),
    )
    .unwrap();
    symlink("undisclosed-absent", dir.join("hidden/absent")).unwrap();
    symlink("undisclosed-directory", dir.join("hidden/directory")).unwrap();
    symlink("undisclosed-loop", dir.join("hidden/loop")).unwrap();
    symlink("undisclosed-loop", dir.join("hidden/undisclosed-loop")).unwrap();

    for policy in ["read: []", "read: [`cwd .], deny: [`cwd hidden]"] {
        for operation in [
            "from-string < hidden/file",
            "from-string < hidden/absolute",
            "exists hidden/absent",
            "file-info hidden/file",
            "list-dir hidden/directory",
            "exists hidden/directory/missing/child",
            "exists hidden/loop/child",
            "echo x > hidden/file",
            "within [dir: hidden/directory] { cwd }",
            "./hidden/file",
            "./hidden/loop/child",
        ] {
            let body = format!("grant [fs: [{policy}]] {{ {operation} }}");
            let error = run_in(&dir, &body).expect_err("the access must be refused");
            let diagnostic = format!("{error:?}");
            assert!(diagnostic.contains("hidden/"), "{body}: {diagnostic}");
            assert!(!diagnostic.contains("undisclosed"), "{body}: {diagnostic}");
            let report = shown(&dir, &format!("audit {{ {body} }}"));
            assert!(!report.contains("undisclosed"), "{body}: {report}");
        }
    }
}

/// `within [dir: d]` enters the directory `d` is: refused where a deny holds
/// it, however spelled, and under an allowance the cwd is its real position.
#[cfg(unix)]
#[test]
fn within_dir_is_refused_under_a_deny_and_enters_the_real_position_under_an_allowance() {
    let (_tmp, dir) = fixture();
    std::os::unix::fs::symlink("deep/dir", dir.join("link")).unwrap();
    for spelling in ["deep/dir", "link"] {
        let body = format!(
            "grant [fs: [read: [`absolute /], deny: [`cwd deep/dir]]] \
             {{ within [dir: {spelling}] {{ cwd }} }}"
        );
        assert!(run_in(&dir, &body).is_err(), "{spelling} must be refused");
    }
    let body = "grant [fs: [read: [`cwd deep]]] { within [dir: link] { cwd } }";
    assert_eq!(
        shown(&dir, body),
        dir.join("deep/dir").display().to_string()
    );
}

/// `list-dir` judges each entry as the listing saw it: a link is a link,
/// whatever it leads to, so a deny on its target does not drop it and a deny
/// on the link does.
#[cfg(unix)]
#[test]
fn list_dir_judges_a_linked_entry_as_the_link() {
    let (_tmp, dir) = fixture();
    std::fs::write(dir.join("secret"), "s").unwrap();
    std::os::unix::fs::symlink("../secret", dir.join("deep/link")).unwrap();
    let under = |deny: &str, body: &str| {
        let body = format!("grant [fs: [read: [`absolute /], deny: [`cwd {deny}]]] {{ {body} }}");
        shown(&dir, &body)
    };
    assert_eq!(under("secret", "length !{list-dir deep}"), "2");
    assert_eq!(under("secret", "!{list-dir deep}[1][type]"), "`symlink");
    assert_eq!(under("deep/link", "length !{list-dir deep}"), "1");
}

/// The discard device is opened by its fixed name, never by a spelling: a
/// redirect to one that folds to `/dev/null` and leads, through a link, to a
/// regular file writes the device and leaves the file be.
#[cfg(unix)]
#[test]
fn a_redirect_through_a_link_spelling_that_folds_to_the_device_writes_the_device() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    // `link`, then enough `..` to fold away `link` and every name of `dir`,
    // and, since the link leads that many levels below `p`, to climb back to
    // `p` through it.
    let ups = dir.components().count();
    let mut deep = dir.join("p");
    deep.extend((0..ups).map(|level| format!("c{level}")));
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir_all(dir.join("p/dev")).unwrap();
    std::fs::write(dir.join("p/dev/null"), b"").unwrap();
    std::os::unix::fs::symlink(&deep, dir.join("link")).unwrap();
    let mut spelling = dir.join("link");
    spelling.extend(std::iter::repeat_n("..", ups));
    spelling.push("dev/null");

    let body = format!("echo swallowed > '{}'", spelling.display());
    run_in(&dir, &body).expect("the device swallows the write");
    assert_eq!(std::fs::read(dir.join("p/dev/null")).unwrap(), b"");
}
