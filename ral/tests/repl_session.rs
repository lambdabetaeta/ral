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

/// A temp `XDG_CONFIG_HOME` holding `ral/rc`, beside an empty `HOME`, so a
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

/// An rc under `XDG_CONFIG_HOME/ral/rc` is found, evaluated, and every part
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
/// keys around it do not survive either.  A *map* rc is refused the same way,
/// statically; the same mistake through a decoded rc, whose type the checker
/// cannot see, is the runtime door's (`rc_unknown_key_in_a_decoded_rc_fails_the_whole_file`).
#[test]
fn rc_bad_literal_key_fails_the_whole_file() {
    let (_dir, env) = rc_home("return [edit_mode: 42, bindings: [okname: 'yes']]");

    let out = repl(&["-i"], &env, "$okname\n");
    assert!(
        out.stderr.contains("skipped, since it does not compile"),
        "the bad literal key must fail the whole rc: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("=> yes"),
        "a failed rc must not apply any of its keys: {}",
        out.stdout
    );
}

/// An unknown key is a static error naming the rc's list — and it is caught
/// through a spread, which a literal-only rule would wave through.
#[test]
fn rc_unknown_key_behind_a_spread_is_a_static_error() {
    let (_dir, env) =
        rc_home("let extra = [surfase: 'minimal', env: [:]]\nreturn [...$extra, env: [:]]");

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("surfase") && out.stderr.contains("skipped, since it does not compile"),
        "the misspelling must be named before the file runs: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("alive"),
        "a broken rc must not strand the user at no shell: {}",
        out.stdout
    );
}

/// An rc returning a *map* is refused before it runs: its keys are the table's
/// labels, so it is written as a record.
#[test]
fn rc_returning_a_map_is_a_static_error() {
    let (_dir, env) = rc_home("return [:, surfase: 'minimal', edit_mode: 'vi']");

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("takes a record of settings")
            && out.stderr.contains("skipped, since it does not compile"),
        "a map rc must be refused statically: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("alive"),
        "a broken rc must not strand the user at no shell: {}",
        out.stdout
    );
}

/// An rc typed at a variable — here decoded from JSON — has no type for the
/// checker to ascribe, so the keyset is met at `apply_rc_config` instead: the
/// key is named, the list is offered, and the whole rc is refused rather than
/// the keys around the bad one landing first.  The shell still starts, with
/// defaults, and the refusal says so.
#[test]
fn rc_unknown_key_in_a_decoded_rc_fails_the_whole_file() {
    let (_dir, env) =
        rc_home(r#"return !{echo '{"surfase": "minimal", "edit_mode": "vi"}' | from-json}"#);

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
        out.stderr.contains("not applied"),
        "the refusal must say the rc did not take effect: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("alive"),
        "a refused rc must not strand the user at no shell: {}",
        out.stdout
    );
}

/// A malformed *value* on a known key, through a decoded rc, refuses the
/// whole file exactly as an unknown key does — agreeing with the record
/// spelling, which fails the same way statically on the same mistake.
#[test]
fn rc_bad_value_in_a_decoded_rc_fails_the_whole_file() {
    let (_dir, env) =
        rc_home(r#"return !{echo '{"edit_mode": 3, "recursion_limit": 4096}' | from-json}"#);

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("'edit_mode'") && out.stderr.contains("must be a string"),
        "the bad value must name the field and what was wrong with it: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("not applied"),
        "the refusal must say the rc did not take effect: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("alive"),
        "a refused rc must not strand the user at no shell: {}",
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
        !out.stderr.contains("skipped, since it does not compile"),
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
        "the alias must actually have run `cd ~`, landing `touch` in the home directory"
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
        "the alias must actually have run `cd ~`, landing `touch` in the home directory"
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

// ── A login session's terminal ─────────────────────────────────────────────

/// A write the shell cannot deliver is dropped, not a panic.  A hung-up
/// terminal fails every write with EIO; an unread stderr pipe stands in for
/// it here, with EPIPE, since the REPL ignores SIGPIPE.
#[cfg(unix)]
#[test]
fn an_undeliverable_diagnostic_is_dropped_not_a_panic() {
    let home = tempfile::tempdir().unwrap();
    let mut child = ral_command()
        .args(["-i", "--norc"])
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    drop(child.stderr.take());
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"no-such-command-anywhere\n").unwrap();
    drop(stdin);
    assert_eq!(
        child.wait().unwrap().code(),
        Some(0),
        "the session must end at EOF, not in a panic"
    );
}

/// A login shell keeps the umask it inherited: login(1) and PAM set it, and
/// the profile is where a user changes it.
#[cfg(unix)]
#[test]
fn a_login_shell_keeps_the_inherited_umask() {
    let home = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "umask 077; exec \"$0\" -l -i --norc"])
        .arg(common::ral_bin())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"/bin/sh -c umask\n").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("0077"),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An interactive shell passes on the signals it inherited ignored: the nohup
/// rule, so a child `sh` run under `nohup ral` survives a SIGHUP to itself.
#[cfg(unix)]
#[test]
fn an_interactive_shell_passes_inherited_ignored_signals_on() {
    let home = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "trap \"\" HUP; exec \"$0\" -i --norc"])
        .arg(common::ral_bin())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"/bin/sh -c 'kill -HUP $$; echo survived'\n")
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("survived"),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── The structural surface refuses an unsized terminal ─────────────────────

/// A pty reporting zero rows, as a harness that never sent `TIOCSWINSZ`
/// leaves it: the structural surface must warn and fall back to readline
/// rather than spin inside ratatui, which never returns on a zero-row screen.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[allow(
    clippy::cast_lossless,
    reason = "TIOCSCTTY's type differs per platform"
)]
fn structural_surface_refuses_zero_sized_terminal() {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let (mut master, slave) = unsafe {
        let (mut m, mut s) = (0, 0);
        let ws = libc::winsize {
            ws_row: 0,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = libc::openpty(
            &raw mut m,
            &raw mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::from_ref(&ws).cast_mut(),
        );
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        (
            std::fs::File::from(OwnedFd::from_raw_fd(m)),
            OwnedFd::from_raw_fd(s),
        )
    };

    let home = tempfile::tempdir().unwrap();
    let mut cmd = ral_command();
    cmd.args(["-i", "--norc", "--surface", "structural"])
        .env("HOME", home.path())
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn ral");
    drop(cmd);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let reader = master.try_clone().unwrap();
    let sink = Arc::clone(&seen);
    std::thread::spawn(move || {
        use std::io::Read;
        let (mut reader, mut chunk) = (reader, [0u8; 4096]);
        while let Ok(n @ 1..) = reader.read(&mut chunk) {
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });

    let start = Instant::now();
    let text_so_far = || String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
    // Once the line editor is reading, Ctrl-D at the empty prompt ends it.
    while !text_so_far().contains("❯") && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    master.write_all(b"\x04").unwrap();
    let exited = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if start.elapsed() > Duration::from_secs(10) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    std::thread::sleep(Duration::from_millis(200));

    let text = text_so_far();
    assert!(exited, "ral never exited on a zero-row terminal:\n{text}");
    assert!(
        text.contains("structural surface unavailable"),
        "no fallback warning:\n{text}"
    );
}
