#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(target_os = "linux")]

mod common;

use ral_core::path::CwdPath;
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
    ending.into_settled().map(drop).map_err(|b| b.to_string())
}

#[test]
fn a_non_utf8_cwd_stays_native_and_its_matches_can_be_relative_text() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join(OsStr::from_bytes(b"cwd\xFF[1]"));
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("ok.txt"), "").unwrap();
    let mut shell = common::fresh_shell();
    shell.seed_cwd(CwdPath::new(cwd).unwrap());
    let result = shell
        .run(Run::captured("glob '*.txt'", SourceOrigin::named("<test>")))
        .expect_ran()
        .into_settled()
        .unwrap();
    assert_eq!(result.to_string(), "[ok.txt]");
}

#[test]
fn a_matching_non_utf8_name_is_refused_and_a_sibling_is_harmless() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(OsStr::from_bytes(b"bad\xFFname.txt")), "x").unwrap();
    std::fs::write(tmp.path().join("ok.txt"), "x").unwrap();
    assert!(
        glob(tmp.path(), "*.txt")
            .unwrap_err()
            .contains("is not UTF-8")
    );
    glob(tmp.path(), "ok*").unwrap();
}
