#![allow(clippy::disallowed_methods, reason = "[test] filesystem fixtures")]

//! The path contracts at the shell boundary: an access is resolved as the
//! kernel resolves it, a grant prefix is a logical spelling, and the root
//! and a name ending in a separator are directories.

mod common;

use ral_core::Value;
use ral_core::protocol::Run;
use ral_core::source::SourceOrigin;
use ral_core::types::Settled;
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

/// A deny on `dlink`, retargeted inside the block by an external `ln` at the
/// file `moved` (named by `printf`), refuses a write through `dlink`.
#[cfg(target_os = "linux")]
fn deny_follows_a_retargeted_link(moved: &[u8], printf: &str) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, dir) = fixture();
    let moved = dir.join(OsStr::from_bytes(moved));
    std::fs::write(&moved, "original").unwrap();
    std::fs::write(dir.join("tgt"), "original").unwrap();
    std::os::unix::fs::symlink("tgt", dir.join("dlink")).unwrap();
    let script = dir.join("retarget.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nln -sfn \"$(printf '{printf}')\" dlink\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let body = "grant [fs: [read: [`absolute /], write: [`cwd .], deny: [`cwd dlink]]] \
                { ./retarget.sh; to-string overwritten > 'dlink' }";
    assert!(run_in(&dir, body).is_err(), "> dlink must be refused");
    assert_eq!(std::fs::read_to_string(&moved).unwrap(), "original");
    assert_eq!(
        std::fs::read_to_string(dir.join("tgt")).unwrap(),
        "original"
    );
}

/// An exclusion holds the live target of its link, whether or not that name
/// is Unicode.
#[cfg(target_os = "linux")]
#[test]
fn a_deny_holds_a_link_retargeted_at_a_utf8_name() {
    deny_follows_a_retargeted_link(b"tgt2", "tgt2");
}

#[cfg(target_os = "linux")]
#[test]
fn a_deny_holds_a_link_retargeted_at_a_name_that_is_not_unicode() {
    deny_follows_a_retargeted_link(b"tgt\xFF", "tgt\\377");
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
