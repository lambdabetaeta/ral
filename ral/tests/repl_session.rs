#![allow(clippy::disallowed_methods)]

//! End-to-end tests of a live REPL session: the rc file on disk, the
//! settings it resolves, the plugins it loads, the worker table the prompt
//! reports, and the `-i`/`-s` precedence that decides whether stdin is a
//! conversation or a script.
//!
//! A non-tty stdin combined with `-i` really does enter the REPL loop, so
//! every test here drives the interpreter through the same path an
//! interactive user does — only the terminal is missing.

mod common;

use common::{Output, ral_command};
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use tempfile::TempDir;

/// Run `ral <args>` with `input` piped on stdin and `envs` overlaid on the
/// inherited environment.  Closing stdin ends the REPL loop.
fn repl(args: &[&str], envs: &[(&str, PathBuf)], input: &str) -> Output {
    let mut cmd = ral_command();
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, val) in envs {
        cmd.env(key, val);
    }

    let mut child = cmd.spawn().expect("spawn ral");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).unwrap();
    drop(stdin);

    let out = child.wait_with_output().unwrap();
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// A temp `$XDG_CONFIG_HOME` holding `ral/rc`, beside an empty `$HOME`, so a
/// spawned session discovers exactly this rc and no profile of the real user.
/// The returned directory must outlive the run.
fn rc_home(rc: &str) -> (TempDir, Vec<(&'static str, PathBuf)>) {
    let dir = tempfile::tempdir().unwrap();
    let (config, home) = (dir.path().join("config"), dir.path().join("home"));
    std::fs::create_dir_all(config.join("ral")).unwrap();
    std::fs::create_dir(&home).unwrap();
    std::fs::write(config.join("ral").join("rc"), rc).unwrap();
    (dir, vec![("XDG_CONFIG_HOME", config), ("HOME", home)])
}

// ── The rc file reaches the session ────────────────────────────────────────

/// An rc under `$XDG_CONFIG_HOME/ral/rc` is found, evaluated, and every part
/// of it observed: the startup block runs, the bindings are in scope, and the
/// theme's `value_prefix` is what the printer uses.  `--norc` on the identical
/// invocation suppresses all of it — which is what proves the first half came
/// from the rc rather than from the defaults.
#[test]
fn rc_theme_bindings_and_startup_reach_a_live_session() {
    let (_dir, env) = rc_home(
        r#"return [theme: [value_prefix: "» "], bindings: [greeting: 'hi-from-rc'], startup: { echo started }]"#,
    );

    let out = repl(&["-i"], &env, "$greeting\n");
    assert!(
        out.stdout.contains("started"),
        "startup block never ran: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("» hi-from-rc"),
        "rc binding under the rc theme's prefix: {}",
        out.stdout
    );

    let bare = repl(&["-i", "--norc"], &env, "let myvar = 41\n$myvar\n");
    assert!(
        bare.stdout.contains("=> 41"),
        "--norc keeps the default prefix: {}",
        bare.stdout
    );
    assert!(
        !bare.stdout.contains("started"),
        "--norc ran the startup block"
    );
}

/// A key the rc gets wrong is reported by name and skipped; the keys around it
/// still apply.  One typo must not disable the whole file.
#[test]
fn rc_bad_key_is_reported_and_the_rest_still_applies() {
    let (_dir, env) = rc_home("return [edit_mode: 42, bindings: [okname: 'yes']]");

    let out = repl(&["-i"], &env, "$okname\n");
    assert!(
        out.stderr
            .contains("ral: rc 'edit_mode' must be a string; got Int"),
        "the bad key must name itself: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("=> yes"),
        "the good key must survive: {}",
        out.stdout
    );
}

/// A plugin named by the rc is loaded before the first prompt, its manifest's
/// alias is dispatchable as a command, and `unload-plugin` removes exactly the
/// bindings that plugin installed.
#[test]
fn rc_plugin_installs_an_alias_and_unload_removes_it() {
    let plug = tempfile::tempdir().unwrap();
    let manifest = plug.path().join("greeter.ral");
    std::fs::write(
        &manifest,
        "return { |options| return [name: 'greeter', aliases: [hail: { |args| echo hail-from-plugin }]] }",
    )
    .unwrap();
    let (_dir, env) = rc_home(&format!(
        "return [plugins: [[plugin: '{}']]]",
        manifest.display()
    ));

    let out = repl(&["-i"], &env, "hail\nunload-plugin greeter\nhail\n");
    assert_eq!(
        out.stdout.matches("hail-from-plugin").count(),
        1,
        "the alias runs before the unload and not after: {}",
        out.stdout
    );
    assert!(
        out.stderr.contains("hail: command not found"),
        "unload must take the alias with it: {}",
        out.stderr
    );
}

// ── Workers at the prompt ──────────────────────────────────────────────────

/// `jobs` renders the session's live workers, and leaving the session reports
/// the ones still running.  Both read the real `Shell::workers()` snapshot.
#[test]
fn jobs_renders_a_live_worker_and_exit_reports_it() {
    let out = repl(&["-i", "--norc"], &[], "let h = spawn { sleep 3 }\njobs\n");
    assert!(
        out.stderr.contains("[w0] running (worker)"),
        "jobs must show the live worker: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("taking down 1 still-running worker"),
        "leaving the session must report the worker: {}",
        out.stderr
    );
}

/// Each job verb names the job it acts on, so a bare one is an arity error
/// before anything runs — and the diagnostic names the verb.  These builtins
/// belong to the REPL host's table, not the core one, so a live session is the
/// only place they are typed at all.
#[test]
fn a_bare_job_verb_is_an_arity_error_naming_the_verb() {
    let out = repl(&["-i", "--norc"], &[], "fg\nbg\ndisown\n");
    for verb in ["fg", "bg", "disown"] {
        let want = format!("`{verb}` expected 1 argument, got 0");
        assert!(
            out.stderr.contains(&want),
            "expected {want:?} in: {}",
            out.stderr
        );
    }
}

/// `fg` on an unknown job does not merely fail: it explains that fg/bg are
/// pgid-only and names the worker-handle eliminator that stands in for it.
#[test]
fn fg_on_an_unknown_job_names_the_handle_correspondence() {
    let out = repl(&["-i", "--norc"], &[], "fg 99\n");
    for want in ["fg: no such job", "pgid-only", "`await`"] {
        assert!(
            out.stderr.contains(want),
            "expected {want:?} in: {}",
            out.stderr
        );
    }
}

// ── `-s` beats `-i` ────────────────────────────────────────────────────────

/// `-s` forces stdin to be read as a batch script even under `-i`.  The two
/// modes are told apart by what only the REPL does: echo each value with the
/// `=> ` prefix, and carry on after an error instead of aborting.
#[test]
fn dash_s_reads_stdin_as_a_script_despite_dash_i() {
    let value = "let myvar = 41\n$myvar\n";
    assert!(
        repl(&["-i", "--norc"], &[], value).stdout.contains("=> 41"),
        "-i alone must enter the REPL"
    );
    assert!(
        !repl(&["-i", "-s", "--norc"], &[], value)
            .stdout
            .contains("=> "),
        "-s must beat -i: a script echoes nothing"
    );

    let after_error = "$nosuchvar\nlet myvar = 41\n$myvar\n";
    let loop_run = repl(&["-i", "--norc"], &[], after_error);
    assert!(
        loop_run.stdout.contains("=> 41") && loop_run.status == 0,
        "the REPL recovers per line: {:?}",
        loop_run.stdout
    );

    let script_run = repl(&["-i", "-s", "--norc"], &[], after_error);
    assert!(
        !script_run.stdout.contains("=> 41") && script_run.status != 0,
        "a script stops at the first error and fails: {:?}",
        script_run.stdout
    );
}
