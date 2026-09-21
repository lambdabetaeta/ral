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

/// A malformed *literal* rc key is a type error, caught before the file
/// runs at all: the whole rc is skipped, not merely that one key — the
/// keys around it do not survive either.
/// `rc_computed_bad_key_is_reported_and_the_rest_still_applies`, below, is
/// the same mistake through a computed rc, where today's tolerant, per-key
/// runtime check is still the only one that can see it.
#[test]
fn rc_bad_literal_key_fails_the_whole_file() {
    let (_dir, env) = rc_home("return [edit_mode: 42, bindings: [okname: 'yes']]");

    let out = repl(&["-i"], &env, "$okname\n");
    assert!(
        out.stderr.contains("skipped due to type errors"),
        "the bad literal key must fail the whole rc: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("=> yes"),
        "a failed rc must not apply any of its keys: {}",
        out.stdout
    );
}

/// The same mistake through a *computed* rc return.  What the contract checks
/// is the inferred row, not the syntax that produced it, so a bound record is
/// held exactly as a written-out one is.
#[test]
fn rc_computed_bad_key_fails_the_whole_file() {
    let (_dir, env) = rc_home("let cfg = [edit_mode: 42, bindings: [okname: 'yes']]\nreturn $cfg");

    let out = repl(&["-i"], &env, "$okname\n");
    assert!(
        out.stderr.contains("skipped due to type errors"),
        "a computed rc carries a row, and the row is checked: {}",
        out.stderr
    );
}

/// An unknown key is a static error naming the rc's list — and it is caught
/// through a spread, which a literal-only rule would wave through.
#[test]
fn rc_unknown_key_behind_a_spread_is_a_static_error() {
    let (_dir, env) = rc_home("let extra = [surfase: 'minimal']\nreturn [...$extra, env: [:]]");

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("surfase") && out.stderr.contains("skipped due to type errors"),
        "the misspelling must be named before the file runs: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("alive"),
        "a broken rc must not strand the user at no shell: {}",
        out.stdout
    );
}

/// An rc returning a *map* has no row to check, so the same keyset is met at
/// `apply_rc_key` instead: the key is named, the list is offered, and the
/// keys around it still apply.
#[test]
fn rc_unknown_key_in_a_mapped_rc_names_the_list_at_run_time() {
    let (_dir, env) = rc_home("return [:, surfase: 'minimal', edit_mode: 'vi']");

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("unknown key 'surfase'"),
        "the bad key must name itself: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("recursion_limit"),
        "the refusal must offer the rc's own list: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("alive"),
        "the rest of the file is applied: {}",
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
        "return [plugins: ['{}': [:]]]",
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

/// Two plugins whose options have nothing in common: one type per key is
/// what a record is for, so the rc's static contract must admit them.  The
/// guard belongs on the boot path because that contract runs nowhere else.
#[test]
fn rc_plugins_take_differently_shaped_options() {
    let plug = tempfile::tempdir().unwrap();
    for alias in ["ping-alpha", "ping-beta"] {
        std::fs::write(
            plug.path().join(format!("{alias}.ral")),
            format!(
                "return {{ |options| return [name: '{alias}', \
                 aliases: [{alias}: {{ |args| echo {alias} }}]] }}"
            ),
        )
        .unwrap();
    }
    let (_dir, env) = rc_home(&format!(
        "return [plugins: ['{}': [key: 'ctrl-t'], '{}': [depth: 3, quiet: true]]]",
        plug.path().join("ping-alpha.ral").display(),
        plug.path().join("ping-beta.ral").display(),
    ));

    let out = repl(&["-i"], &env, "ping-alpha\nping-beta\n");
    assert!(
        !out.stderr.contains("skipped due to type errors"),
        "the rc must survive its own contract check: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("ping-alpha") && out.stdout.contains("ping-beta"),
        "both plugins must load: {}{}",
        out.stdout,
        out.stderr
    );
}

// ── The head pin admits `Value Unit ⊑ Bytes` ────────────────────────────────

/// An alias body ending in a value-routed, `Unit`-returning builtin (`cd`)
/// installs under a fresh name with no trailing byte-write: the head pin
/// (`pin_arm_to_head`) now shares the arm-join's subsumption instance,
/// `Value Unit ⊑ Bytes`, rather than demanding the arm write a byte itself.
#[test]
fn an_alias_ending_in_cd_installs_with_no_trailing_write() {
    let (dir, env) = rc_home("return [aliases: [gohome: { |args| cd ~ }]]");
    let marker = env[1].1.join("it-worked");
    let out = repl(&["-i"], &env, "gohome\ntouch it-worked\n");
    assert!(
        !out.stderr.contains("ralrc alias"),
        "an alias ending in `cd` must install; stderr was:\n{}",
        out.stderr
    );
    assert!(
        marker.exists(),
        "the alias must actually have run `cd ~`, landing `touch` in $HOME"
    );
    drop(dir);
}

/// The same, where the arm is an `if` whose branches are *both* ground
/// `Value` (so the join lands on the value side, per `conclude_value_side`)
/// before the whole arm pins to the head's `Bytes` route.
#[test]
fn an_alias_whose_if_join_is_all_value_still_installs() {
    let (dir, env) = rc_home(
        "return [aliases: [gohome: { |args| \
             if !{is-empty $args} { cd ~ } else { cd ~ } }]]",
    );
    let marker = env[1].1.join("it-worked");
    let out = repl(&["-i"], &env, "gohome\ntouch it-worked\n");
    assert!(
        !out.stderr.contains("ralrc alias"),
        "an if/else of two `cd`s must install; stderr was:\n{}",
        out.stderr
    );
    assert!(
        marker.exists(),
        "the alias must actually have run `cd ~`, landing `touch` in $HOME"
    );
    drop(dir);
}

/// An arm that genuinely returns a payload — a `String`, under a head with
/// no prior handler — is still rejected: the subsumption only ever admits
/// `Value Unit`, never a value that survives to the boundary.
#[test]
fn an_alias_returning_a_string_is_still_rejected() {
    let (_dir, env) = rc_home("return [aliases: [greet: { |args| \"hi\" }]]");
    let out = repl(&["-i"], &env, "echo ok\n");
    assert!(
        out.stderr.contains("ralrc alias")
            && out.stderr.contains("greet")
            && out.stderr.contains("String"),
        "a value-returning arm over a fresh name must still fail; stderr was:\n{}",
        out.stderr
    );
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
