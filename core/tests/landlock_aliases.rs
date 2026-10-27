#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(feature = "test-util", target_os = "linux"))]

//! Landlock judges aliases encountered during subtraction by inode.

mod common;

use ral_core::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
use ral_core::path::FrozenPath;
use ral_core::test_helper::confinement::{admitted, denied, run};
use ral_core::test_helper::{layer, stack};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

const NAMESPACE: &str = "RAL_TEST_LANDLOCK_ALIASES";
const TEST: &str = "encountered_bind_aliases_preserve_exec_exclusions";

fn bind(source: &Path, target: &Path) -> bool {
    let source = CString::new(source.as_os_str().as_bytes()).unwrap();
    let target = CString::new(target.as_os_str().as_bytes()).unwrap();
    // SAFETY: both paths are NUL-terminated; MS_BIND needs no data.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } == 0
    {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES)) {
        ral_core::errln!(
            "skip {TEST}: private namespace refused bind {source:?} at {target:?}: {error}"
        );
        return false;
    }
    panic!("cannot bind {source:?} at {target:?}: {error}");
}

fn exercise(root: &Path) {
    for (source, target) in [
        ("holding", "holding/loop"),
        ("holding/excluded", "listed/excluded-alias"),
        ("holding", "listed/holding-alias"),
    ] {
        if !bind(&root.join(source), &root.join(target)) {
            return;
        }
    }
    let proof = ral_core::require_confinement!();
    ral_core::require_landlock!(1);
    let frozen = |name: &str| FrozenPath::freeze(&root.join(name)).unwrap();
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(root).unwrap()],
            ..FsPolicy::default()
        }),
        exec: Some(ExecGrant::of(
            [
                ExecKey::Path(FrozenPath::from_surface("/bin/sh")),
                ExecKey::Dir(FrozenPath::freeze(root).unwrap()),
            ],
            [
                ExecKey::Dir(frozen("holding/excluded")),
                ExecKey::Path(frozen("holding/blocked")),
                ExecKey::Path(frozen("listed/blocked")),
            ],
        )),
        ..Capabilities::default()
    };
    let grants = stack([layer(caps)]);
    let launch = |name: &str| {
        run(
            grants.clone(),
            &format!(
                "/bin/sh -c 'exec \"$1\" ok' sh '{}'",
                root.join(name).display()
            ),
        )
    };
    let control = launch("control/ok");
    admitted(&proof, &control, "ok");
    for prefix in ["holding", "holding/loop", "listed/holding-alias"] {
        let allowed = launch(&format!("{prefix}/ok"));
        admitted(&proof, &allowed, "ok");
        for suffix in ["excluded/tool", "blocked"] {
            let refused = launch(&format!("{prefix}/{suffix}"));
            denied(&proof, &allowed, &refused, "ok");
        }
    }
    let listed = launch("listed/ok");
    admitted(&proof, &listed, "ok");
    for name in [
        "listed/excluded-alias/tool",
        "listed/blocked",
        "listed/file-alias",
    ] {
        denied(&proof, &listed, &launch(name), "ok");
    }
}

#[test]
fn encountered_bind_aliases_preserve_exec_exclusions() {
    if let Some(root) = std::env::var_os(NAMESPACE) {
        exercise(Path::new(&root));
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for dir in [
        "holding/excluded",
        "holding/loop",
        "listed/excluded-alias",
        "listed/holding-alias",
        "control",
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    for name in [
        "holding/ok",
        "holding/blocked",
        "holding/excluded/tool",
        "listed/ok",
        "listed/blocked",
        "control/ok",
    ] {
        std::fs::copy("/bin/echo", root.join(name)).unwrap();
    }
    std::fs::hard_link(root.join("holding/blocked"), root.join("listed/file-alias")).unwrap();
    let out = Command::new("unshare")
        .args(["--mount", "--propagation", "private"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(NAMESPACE, root)
        .env("LC_ALL", "C")
        .output();
    let out = match out {
        Ok(out) => out,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ral_core::errln!("skip {TEST}: unshare is unavailable: {error}");
            return;
        }
        Err(error) => panic!("cannot start the private mount namespace: {error}"),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success()
        && stderr.starts_with("unshare: unshare failed:")
        && (stderr.contains("Operation not permitted") || stderr.contains("Permission denied"))
    {
        ral_core::errln!("skip {TEST}: private mount namespace refused: {stderr}");
        return;
    }
    assert!(
        out.status.success(),
        "private namespace test failed ({}):\nstdout: {}\nstderr: {stderr}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
    );
    eprint!("{stderr}");
}
