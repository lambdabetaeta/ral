//! End-to-end tests of a live REPL session: the rc file on disk, the
//! settings it resolves, the plugins it loads, the worker table the prompt
//! reports, and the `-i`/`-s` precedence that decides whether stdin is a
//! conversation or a script.
//!
//! A non-tty stdin combined with `-i` really does enter the REPL loop, so
//! every test here drives the interpreter through the same path an
//! interactive user does — only the terminal is missing.

#[cfg(unix)]
use crate::common::{ral_command, repl_output_lines};
use crate::common::{rc_home, repl};
use ral_core::test_helper::rc_source;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::process::Stdio;

// ── The rc file reaches the session ────────────────────────────────────────

/// An rc under `XDG_CONFIG_HOME/ral/rc` is found, evaluated, and every part
/// of it observed: the startup block runs, the bindings are in scope, and the
/// theme's `value-prefix` is what the printer uses.  `--norc` on the identical
/// invocation suppresses all of it — which is what proves the first half came
/// from the rc rather than from the defaults.
#[test]
fn rc_theme_bindings_and_startup_reach_a_live_session() {
    let (_dir, env) = rc_home(&format!(
        "let greeting = 'hi-from-rc'\n{}",
        rc_source(&[
            (
                "theme",
                r#"[value-prefix: `set "» ", value-color: `inherit]"#,
            ),
            ("startup", "`set { echo started }"),
        ])
    ));

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
/// runs at all: the whole rc is skipped, including its earlier `let`s.
#[test]
fn rc_bad_literal_key_fails_the_whole_file() {
    let (_dir, env) = rc_home(&format!(
        "let okname = 'yes'\n{}",
        rc_source(&[("edit-mode", "42")])
    ));

    let out = repl(&["-i"], &env, "$okname\n");
    assert!(
        out.stderr.contains("skipped, since it does not compile"),
        "the bad literal key must fail the whole rc: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("=> yes"),
        "a refused rc must not install its top-level lets: {}",
        out.stdout
    );
}

#[test]
fn empty_rc_reports_its_path_and_keeps_the_shell_live() {
    let (_dir, env) = rc_home("");
    let rc = env[0].1.join("ral/rc");

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("[T0081]")
            && out.stderr.contains("`rc` is missing required keys:")
            && out
                .stderr
                .contains(&format!("{}:1:1:", ral_core::path::shown(&rc)))
            && out.stderr.contains("skipped, since it does not compile"),
        "the empty rc must be refused at its own location: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("alive"),
        "an empty rc must not strand the user at no shell: {}",
        out.stdout
    );
}

/// A spread hides the rc's keys, so it is refused before the file runs.
#[test]
fn rc_ending_in_a_spread_is_a_static_error() {
    let (_dir, env) =
        rc_home("let extra = [surfase: 'minimal', env: [:]]\nreturn [...$extra, env: [:]]");

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("[T0077]")
            && out.stderr.contains("a spread hides which keys it brings")
            && out.stderr.contains("skipped, since it does not compile"),
        "the spread must be refused before the file runs: {}",
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
        out.stderr.contains("[T0077]")
            && out
                .stderr
                .contains("`rc`'s keys are labels: write a record")
            && out.stderr.contains("not a map")
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

/// A computed rc tail is refused statically; even earlier effects never run.
#[test]
fn rc_ending_in_a_computed_value_is_a_static_error() {
    let (_dir, env) = rc_home(
        r#"echo must-not-run
return !{echo '{"edit-mode": "vi"}' | from-json}"#,
    );

    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("[T0077]")
            && out.stderr.contains("a computed value hides its keys")
            && out.stderr.contains("skipped, since it does not compile"),
        "a computed rc must be refused before it runs: {}",
        out.stderr
    );
    assert!(!out.stdout.contains("must-not-run"), "{out:?}");
    assert!(
        out.stdout.contains("alive"),
        "a refused rc must not strand the user at no shell: {}",
        out.stdout
    );
}

/// A plugin named by the rc is loaded before the first prompt, its manifest's
/// handler is dispatchable as a command.
#[test]
fn rc_plugin_installs_a_handler() {
    let plug = tempfile::tempdir().unwrap();
    let manifest = plug.path().join("greeter.ral");
    std::fs::write(
        &manifest,
        "return { |options| return [name: 'greeter', handlers: [:, hail: { |args| echo hail-from-plugin }]] }",
    )
    .unwrap();
    let (_dir, env) = rc_home(&rc_source(&[(
        "plugins",
        &format!("['{}': [:]]", manifest.display()),
    )]));

    let out = repl(&["-i"], &env, "hail\n");
    assert_eq!(
        out.stdout.matches("hail-from-plugin").count(),
        1,
        "the startup handler runs: {}{}",
        out.stdout,
        out.stderr
    );
}

/// A written factory's handler keeps the helper captured from its file scope.
#[test]
fn a_plugin_handler_calls_a_helper_its_scope_held() {
    let plug = tempfile::tempdir().unwrap();
    let manifest = plug.path().join("greeter.ral");
    std::fs::write(
        &manifest,
        "let greet = { |word args| echo $word ...$args }\n\
         return { |options| [name: 'greeter', handlers: [:, hail: { greet hail-from-plugin }]] }\n",
    )
    .unwrap();
    let (_dir, env) = rc_home(&rc_source(&[(
        "plugins",
        &format!("['{}': [:]]", manifest.display()),
    )]));
    let out = repl(&["-i"], &env, "hail there\n");
    assert!(
        out.stdout.contains("hail-from-plugin there"),
        "the written factory loads and its handler runs: {}{}",
        out.stdout,
        out.stderr
    );
}

/// Two plugins whose options have nothing in common: one type per key is
/// what a record is for, so the rc's static contract must admit them.  The
/// guard belongs on the boot path because that contract runs nowhere else.
#[test]
fn rc_plugins_take_differently_shaped_options() {
    let plug = tempfile::tempdir().unwrap();
    for handler in ["ping-alpha", "ping-beta"] {
        std::fs::write(
            plug.path().join(format!("{handler}.ral")),
            format!(
                "return {{ |options| return [name: '{handler}', \
                 handlers: [:, {handler}: {{ |args| echo {handler} }}]] }}"
            ),
        )
        .unwrap();
    }
    let (_dir, env) = rc_home(&rc_source(&[(
        "plugins",
        &format!(
            "['{}': [key: 'ctrl-t'], '{}': [depth: 3, quiet: true]]",
            plug.path().join("ping-alpha.ral").display(),
            plug.path().join("ping-beta.ral").display(),
        ),
    )]));

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

// ── Contract arms stand in for their heads ────────────────────────────────

/// A handler body ending in `cd` checks and installs without a trailing write:
/// its fresh-grade `Unit` result unifies with `Command`.
#[test]
fn a_handler_ending_in_cd_installs_with_no_trailing_write() {
    let (dir, env) = rc_home(&rc_source(&[("handlers", "[:, gohome: { |args| cd ~ }]")]));
    let marker = env[1].1.join("it-worked");
    let out = repl(&["-i"], &env, "gohome\ntouch it-worked\n");
    assert!(
        marker.exists(),
        "the handler must actually have run `cd ~`, landing `touch` in the home directory; stderr:\n{}",
        out.stderr
    );
    drop(dir);
}

/// An `if` with `cd` in both branches also has a fresh-grade `Unit` result
/// that unifies with `Command`.
#[test]
fn a_handler_whose_if_join_is_all_value_still_installs() {
    let (dir, env) = rc_home(&rc_source(&[(
        "handlers",
        "[:, gohome: { |args| if !{is-empty $args} { cd ~ } else { cd ~ } }]",
    )]));
    let marker = env[1].1.join("it-worked");
    let out = repl(&["-i"], &env, "gohome\ntouch it-worked\n");
    assert!(
        marker.exists(),
        "the handler must actually have run `cd ~`, landing `touch` in the home directory; stderr:\n{}",
        out.stderr
    );
    drop(dir);
}

/// An arm returning a `String` under an external head is refused by the rc's
/// check, before even its first phrase runs, not by handler installation.
#[test]
fn a_handler_returning_a_string_is_still_rejected() {
    let rc = format!(
        "echo must-not-run\n{}",
        rc_source(&[("handlers", "[:, greet: { |args| \"hi\" }]")])
    );
    let (_dir, env) = rc_home(&rc);
    let out = repl(&["-i"], &env, "echo ok\n");
    assert!(
        out.stderr
            .contains("an arm for `greet` stands in for a command")
            && out.stderr.contains("String")
            && out.stderr.contains("skipped, since it does not compile"),
        "a value-returning arm over a fresh name must still fail; stderr was:\n{}",
        out.stderr
    );
    assert!(!out.stdout.contains("must-not-run"), "{out:?}");
    assert!(out.stdout.contains("ok"), "{out:?}");
}

#[test]
fn a_written_non_block_rc_handler_is_refused_before_sourcing() {
    let rc = format!(
        "echo must-not-run\n{}",
        rc_source(&[("handlers", "[:, greet: 3]")])
    );
    let (_dir, env) = rc_home(&rc);
    let out = repl(&["-i"], &env, "echo alive\n");
    assert!(
        out.stderr.contains("skipped, since it does not compile"),
        "{out:?}"
    );
    assert!(out.stderr.contains("greet: 3"), "{out:?}");
    assert!(!out.stderr.contains("not a function"), "{out:?}");
    assert!(!out.stdout.contains("must-not-run"), "{out:?}");
    assert!(out.stdout.contains("alive"), "{out:?}");
}

#[test]
fn written_factory_handlers_are_refused_before_the_plugin_runs() {
    for arm in ["3", "{ |args| return 5 }"] {
        let plug = tempfile::tempdir().unwrap();
        let path = plug.path().join("plugin.ral");
        std::fs::write(
            &path,
            format!(
                "echo must-not-run\nreturn {{ |options| return [name: 'p', handlers: [:, greet: {arm}]] }}"
            ),
        )
        .unwrap();
        let rc = format!(
            "let plugin = return {}\n{}",
            ral_core::syntax::quote_word(&ral_core::path::shown(&path)),
            rc_source(&[("plugins", "[$plugin: [:]]")])
        );
        let (_dir, env) = rc_home(&rc);
        let out = repl(&["-i"], &env, "echo alive\n");
        assert!(
            out.stderr.contains("skipped, since it does not compile"),
            "{arm}: {out:?}"
        );
        assert!(out.stderr.contains(&format!("greet: {arm}")), "{out:?}");
        assert!(!out.stdout.contains("must-not-run"), "{out:?}");
        assert!(out.stdout.contains("alive"), "{out:?}");
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
/// the rc's `umask:` is where a user changes it.
#[cfg(unix)]
#[test]
fn a_login_shell_keeps_the_inherited_umask() {
    let home = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "umask 077; exec \"$0\" -l -i --norc"])
        .arg(crate::common::ral_bin())
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
        .arg(crate::common::ral_bin())
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

#[cfg(unix)]
#[test]
fn rc_umask_applies_to_children_and_rals_own_creates() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, env) = rc_home(&rc_source(&[("umask", "`set '077'")]));
    let file = env[1].1.join("created");
    let out = repl(
        &["-i"],
        &env,
        &format!("/bin/sh -c umask\necho x > '{}'\n", file.display()),
    );
    assert!(
        repl_output_lines(&out.stdout).any(|l| l == "0077"),
        "{out:?}"
    );
    assert_eq!(
        std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn rc_open_file_limit_reaches_children() {
    let (_dir, env) = rc_home(&rc_source(&[(
        "limits",
        "[open-files: `set 64, core-bytes: `inherit, stack-bytes: `inherit]",
    )]));
    let out = repl(&["-i"], &env, "/bin/sh -c 'ulimit -n'\n");
    assert!(repl_output_lines(&out.stdout).any(|l| l == "64"), "{out:?}");
}

#[cfg(unix)]
#[test]
fn grant_core_limit_wins_over_rc_session_limit() {
    let (_dir, env) = rc_home(&rc_source(&[(
        "limits",
        "[open-files: `inherit, core-bytes: `set 1048576, stack-bytes: `inherit]",
    )]));
    let out = repl(
        &["-i"],
        &env,
        "grant [exec: [allow: [`absolute /bin/sh]]] { /bin/sh -c 'ulimit -c' }\n",
    );
    assert!(repl_output_lines(&out.stdout).any(|l| l == "0"), "{out:?}");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_repl_announces_an_unclaimed_worker_once() {
    use crate::common::pty::PtySession;
    use std::time::{Duration, Instant};

    let mut session = PtySession::spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let prompt = |session: &mut PtySession, count| {
        assert!(
            session.wait_until(deadline.saturating_duration_since(Instant::now()), |text| {
                text.matches('❯').count() >= count
            }),
            "no prompt #{count}: {}",
            session.text()
        );
    };
    prompt(&mut session, 1);
    session
        .send_line("let h = spawn \"f\" { fail boom }")
        .unwrap();
    let mut prompts = 1;
    loop {
        prompts += 1;
        prompt(&mut session, prompts);
        if session.text().contains("note: worker #") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no worker notice: {}",
            session.text()
        );
        // A new prompt observes deferred delivery without claiming the result.
        session.send_line("").unwrap();
    }
    for line in ["echo a", "echo b"] {
        session.send_line(line).unwrap();
        prompts += 1;
        prompt(&mut session, prompts);
    }
    session.send_line("exit").unwrap();
    assert!(
        session
            .wait_for_exit(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
            .success()
    );
    let out = session.text();
    let notes: Vec<_> = out
        .lines()
        .filter(|line| line.contains("note: worker #"))
        .collect();
    assert_eq!(notes.len(), 1, "{out}");
    assert!(notes[0].contains("(f) failed: boom"), "{out}");
    assert!(!out.contains("dropped a surfaced value"), "{out}");
}

#[cfg(unix)]
#[test]
fn a_repl_never_announces_a_claimed_worker() {
    // Claim before the next prompt's announcement gate.
    let out = repl(
        &["-i", "--norc"],
        &[],
        "let a = spawn \"a\" { return 1 }; await $a\nlet b = spawn \"b\" { ^sleep 5 }; cancel $b\n^sleep 1\necho next\n",
    );
    assert_eq!(out.status, 0, "{out:?}");
    assert!(out.stdout.contains("next"), "{out:?}");
    for channel in [&out.stdout, &out.stderr] {
        assert!(!channel.contains("note: worker #"), "{out:?}");
        assert!(!channel.contains("dropped a surfaced value"), "{out:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_repl_quit_takes_a_running_worker_down() {
    let start = std::time::Instant::now();
    let out = repl(
        &["-i", "--norc"],
        &[],
        "let h = spawn \"t\" { ^sleep 30 }\nexit\n",
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert!(
        out.stderr.contains("taking down 1 worker still running: #"),
        "{out:?}"
    );
    assert!(out.stderr.contains("(t)"), "{out:?}");
}
