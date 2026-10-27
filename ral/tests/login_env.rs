#![cfg(unix)]
#![allow(clippy::disallowed_methods, reason = "test code")]

mod common;

use common::{Output, ral_command, repl_output_lines};
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

fn login(profile: &str, flags: &[&str], input: &str) -> Output {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".profile"), profile).unwrap();
    let stdout = tempfile::tempfile().unwrap();
    let stderr = tempfile::tempfile().unwrap();
    let mut child = ral_command()
        .args(["-l", "-i", "--surface", "minimal"])
        .args(flags)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("RAL_DROP_ME", "x")
        .env("SHLVL", "7")
        .env_remove("RAL_LOGIN_PROBE")
        .stdin(Stdio::piped())
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("login did not finish within five seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |mut file: std::fs::File| {
        use std::io::{Read, Seek};
        file.rewind().unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text
    };
    Output {
        stdout: read(stdout),
        stderr: read(stderr),
        status: status.code().unwrap_or(1),
    }
}

const PROFILE: &str = "export RAL_LOGIN_PROBE=from-profile\nexport PATH=/ral-probe/bin:$PATH\n";

#[test]
fn login_adopts_new_and_changed_variables() {
    let out = login(
        PROFILE,
        &[],
        "echo !{env}[RAL_LOGIN_PROBE]\necho !{env}[PATH]\n",
    );
    assert_eq!(out.status, 0, "{out:?}");
    assert!(
        repl_output_lines(&out.stdout).any(|l| l == "from-profile"),
        "{out:?}"
    );
    assert!(
        repl_output_lines(&out.stdout).any(|l| l.starts_with("/ral-probe/bin:")),
        "{out:?}"
    );
}

#[test]
fn login_adopts_removed_variables() {
    let out = login(
        "unset RAL_DROP_ME\n",
        &[],
        "echo !{has !{env} RAL_DROP_ME}\n",
    );
    assert!(repl_output_lines(&out.stdout).any(|l| l == "false"), "{out:?}");
}

#[test]
fn login_increments_only_rals_shlvl() {
    let out = login("", &[], "echo !{env}[SHLVL]\n");
    assert!(repl_output_lines(&out.stdout).any(|l| l == "8"), "{out:?}");
}

#[test]
fn failed_profile_warns_and_keeps_the_session() {
    let out = login("exit 3\n", &[], "echo ok\n");
    assert_eq!(out.status, 0, "{out:?}");
    assert!(repl_output_lines(&out.stdout).any(|l| l == "ok"), "{out:?}");
    assert!(
        out.stderr.contains("was not adopted") && out.stderr.contains("status 3"),
        "{out:?}"
    );
}

#[test]
fn norc_skips_login_adoption() {
    let out = login(PROFILE, &["--norc"], "echo !{has !{env} RAL_LOGIN_PROBE}\n");
    assert!(repl_output_lines(&out.stdout).any(|l| l == "false"), "{out:?}");
}

#[test]
fn background_profile_job_cannot_hold_the_report_open() {
    let out = login("sleep 30 &\n", &[], "echo reached-prompt\n");
    assert_eq!(out.status, 0, "{out:?}");
    assert!(
        repl_output_lines(&out.stdout).any(|l| l == "reached-prompt"),
        "{out:?}"
    );
}
