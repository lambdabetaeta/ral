#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(target_os = "linux")]

mod common;

use ral_core::protocol::Run;
use ral_core::shell::run::RunReport;
use ral_core::source::SourceOrigin;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

fn glob(dir: &std::path::Path, pattern: &str) -> Result<(), String> {
    let mut shell = common::fresh_shell();
    let source = format!("glob '{}/{pattern}'", dir.display());
    let RunReport::Ran { ending, .. } =
        shell.run(Run::captured(source, SourceOrigin::named("<test>")))
    else {
        panic!("the glob must compile");
    };
    ending.into_result().map(drop).map_err(|b| format!("{b:?}"))
}

#[test]
fn a_matching_non_utf8_name_is_refused_and_a_sibling_is_harmless() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(OsStr::from_bytes(b"bad\xFFname.txt")), "x").unwrap();
    std::fs::write(tmp.path().join("ok.txt"), "x").unwrap();
    assert!(
        glob(tmp.path(), "*.txt")
            .unwrap_err()
            .contains("not valid UTF-8")
    );
    glob(tmp.path(), "ok*").unwrap();
}
