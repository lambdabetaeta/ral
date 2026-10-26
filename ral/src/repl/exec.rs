//! One input line, dispatched through the engine protocol.
//!
//! [`step`] runs a source line through the transport with the REPL host,
//! drains to its [`Report`], and fires the lifecycle hooks around it — each a
//! dispatch of its own.  `pre-exec` comes before and is folded over the line,
//! each hook free to rewrite it; after, `chpwd` if the line moved the
//! session's directory, then `post-exec`, which sees the line that ran.

use ral_core::carrier::Transport;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::protocol::{Ending, Report, Run};
use ral_core::source::SourceOrigin;
use ral_core::{HookName, Value, shell::builtins};
use ral_core::{err, errln, outln};
use std::sync::Arc;

use super::errfmt::format_plugin_warning;
use super::host::{Outcome, ReplHost};
use super::plugin::lock;
use super::plugin::manifest::HookEvent;
use super::theme::OutputTheme;

pub(super) enum Step {
    Continue,
    Exit(u8),
}

fn print_result(val: &Value, theme: &OutputTheme) {
    match val {
        Value::Unit => {}
        Value::Bytes(b) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b);
        }
        _ => {
            let s = match val {
                Value::List(_) | Value::Map(_) => {
                    builtins::pretty_print(val, 0, &builtins::REPL_PRINT_PARAMS)
                }
                _ => val.to_string(),
            };
            if ral_core::io::terminal::ui_color()
                && let Some(color) = &theme.value_color
            {
                outln!(
                    "{color}{}{s}{}",
                    theme.value_prefix,
                    ral_core::io::ansi::RESET
                );
            } else {
                outln!("{}{s}", theme.value_prefix);
            }
        }
    }
}

fn record(entries: Vec<(&str, FOValue)>) -> FOValue {
    FOValue::Map {
        entries: entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    }
}

/// An input line's run: the terminal leased, as a foreground command has it.
pub(super) fn line_run(src: &str) -> Run {
    Run::foreground(src, SourceOrigin::named("<stdin>"))
}

/// Fire lifecycle `event` on every plugin that registers it.  Every
/// lifecycle hook receives exactly one argument, the event record:
/// `{src, status}` for `post-exec`, `{old, new}` for `chpwd`.
fn fire(t: &dyn Transport, host: &Arc<ReplHost>, event: HookEvent, record: &FOValue) {
    let names = lock(&host.runtime).with_hook(event);
    for name in names {
        let hr = host.run_hook(
            t,
            HookName::plugin(name, <&str>::from(event)),
            vec![record.clone()],
            None,
            None,
        );
        // Printed, not deferred: a lifecycle hook runs with the terminal
        // already stable, and a deferred message would wait for a next prompt
        // the last command before exit never reaches.
        if let Outcome::Faulted(fault) | Outcome::Walled(fault) | Outcome::Stopped(fault) =
            hr.outcome
        {
            errln!("{fault}");
        }
    }
}

/// Fold the plugins' `pre-exec` hooks over `src` in load order: a hook returns
/// the line to run, or `()` to leave it.  A fault is reported and passed over,
/// so no hook can stop every line, `unload-plugin` itself included.
fn pre_exec(t: &dyn Transport, host: &Arc<ReplHost>, src: String) -> String {
    let names = lock(&host.runtime).with_hook(HookEvent::PreExec);
    names.into_iter().fold(src, |src, name| {
        let hr = host.run_hook(
            t,
            HookName::plugin(&name, <&str>::from(HookEvent::PreExec)),
            vec![record(vec![("src", src.clone().encode())])],
            None,
            None,
        );
        match hr.outcome {
            Outcome::Faulted(fault) | Outcome::Walled(fault) | Outcome::Stopped(fault) => {
                errln!("{fault}");
                src
            }
            Outcome::Settled(FOValue::String { value }) => value,
            Outcome::Settled(FOValue::Unit) | Outcome::Exited => src,
            Outcome::Settled(other) => {
                errln!(
                    "{}",
                    format_plugin_warning(
                        &name,
                        &format!(
                            "its pre-exec hook returned {}; a pre-exec hook returns the line to run, or `()`",
                            other.shape()
                        ),
                    )
                );
                src
            }
        }
    })
}

/// Dispatch one trimmed non-empty input line between its lifecycle hooks;
/// `post-exec` follows every line, a static failure's included.
///
/// The session is the working directory's outermost handler, so `chpwd`
/// compares its directory across the line: a `cd` that a `within [dir: …]`
/// undid, or a `cd .`, fires nothing.
pub(super) fn step(
    trimmed: &str,
    t: &dyn Transport,
    host: &Arc<ReplHost>,
    theme: &OutputTheme,
) -> Step {
    let src = pre_exec(t, host, trimmed.to_owned());
    let before = t.cwd().ok().flatten();

    let (report, _) = host.dispatch(t, line_run(&src), None);
    let (status, step) = match report {
        Err(severed) => {
            errln!("ral: {severed}");
            return Step::Exit(1);
        }
        Ok(Report::Static { rendered, status } | Report::Faulted { rendered, status }) => {
            err!("{rendered}");
            (status.get(), Step::Continue)
        }
        Ok(Report::Ran { ending, .. }) => {
            let status = ending.status();
            let step = match ending {
                Ending::Settled { value, .. } => {
                    print_result(&Value::from(value), theme);
                    Step::Continue
                }
                Ending::Raised { rendered, .. }
                | Ending::Stopped { rendered, .. }
                | Ending::Unreturnable { rendered, .. } => {
                    err!("{rendered}");
                    Step::Continue
                }
                Ending::Exited(code) => Step::Exit(crate::platform::exit_byte(code)),
            };
            (status, step)
        }
    };

    if let (Some(old), Ok(Some(new))) = (before, t.cwd())
        && old != new
    {
        let path = |p: std::path::PathBuf| ral_core::path::shown(p).encode();
        fire(
            t,
            host,
            HookEvent::Chpwd,
            &record(vec![("old", path(old)), ("new", path(new))]),
        );
    }
    fire(
        t,
        host,
        HookEvent::PostExec,
        &record(vec![
            ("src", src.encode()),
            (
                "status",
                FOValue::Int {
                    value: status.into(),
                },
            ),
        ]),
    );
    step
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::repl::plugin::load::harness::dressed;
    use ral_core::carrier::IdentityTransport;

    fn run(line: &str, t: &IdentityTransport, host: &Arc<ReplHost>) {
        step(line, t, host, &OutputTheme::default());
    }

    /// `post-exec` hands the handler one event record carrying the source
    /// line under `src` and the line's status under `status`.
    #[test]
    fn post_exec_passes_src_and_status_in_one_event_record() {
        let (t, host, sink) = dressed(&[(
            "p",
            HookEvent::PostExec,
            "{ |ev| record $ev[src] $ev[status] }",
        )]);
        let line = "fail [status: 7, message: 'x']";
        run(line, &t, &host);
        assert_eq!(
            *sink.lock().unwrap(),
            vec![Value::string(line), Value::Int(7)]
        );
    }

    /// A static failure still closes its `pre-exec` with a `post-exec`.
    #[test]
    fn post_exec_fires_after_a_static_failure() {
        let (t, host, sink) = dressed(&[("p", HookEvent::PostExec, "{ |ev| record $ev[status] }")]);
        run("if", &t, &host);
        assert_eq!(sink.lock().unwrap().len(), 1, "one post-exec per line");
    }

    /// `pre-exec` hands the handler one event record carrying the source
    /// line under `src`.
    #[test]
    fn pre_exec_passes_src_in_one_event_record() {
        let (t, host, sink) = dressed(&[("p", HookEvent::PreExec, "{ |ev| record $ev[src] }")]);
        run("return ()", &t, &host);
        assert_eq!(*sink.lock().unwrap(), vec![Value::string("return ()")]);
    }

    /// The line a `pre-exec` hook returns is the line that runs.
    #[test]
    fn pre_exec_rewrite_is_what_runs() {
        let (t, host, sink) = dressed(&[(
            "p",
            HookEvent::PreExec,
            "{ |_ev| return 'record rewritten' }",
        )]);
        run("return ()", &t, &host);
        assert_eq!(*sink.lock().unwrap(), vec![Value::string("rewritten")]);
    }

    /// Each `pre-exec` hook sees the line the one before it returned.
    #[test]
    fn pre_exec_rewriters_compose_in_load_order() {
        let (t, host, sink) = dressed(&[
            ("p", HookEvent::PreExec, "{ |ev| return \"$ev[src] one\" }"),
            ("q", HookEvent::PreExec, "{ |ev| return \"$ev[src] two\" }"),
        ]);
        run("record", &t, &host);
        assert_eq!(
            *sink.lock().unwrap(),
            vec![Value::string("one"), Value::string("two")]
        );
    }

    /// A failing `pre-exec` hook is reported, and the line runs as typed.
    #[test]
    fn failing_pre_exec_does_not_stop_the_line() {
        let (t, host, sink) = dressed(&[(
            "p",
            HookEvent::PreExec,
            "{ |_ev| fail [status: 1, message: 'boom'] }",
        )]);
        run("record ran", &t, &host);
        assert_eq!(*sink.lock().unwrap(), vec![Value::string("ran")]);
    }

    /// A `pre-exec` hook returning neither a line nor `()` leaves the line.
    #[test]
    fn wrong_shaped_pre_exec_return_leaves_the_line() {
        let (t, host, sink) = dressed(&[("p", HookEvent::PreExec, "{ |_ev| return 7 }")]);
        run("record typed", &t, &host);
        assert_eq!(*sink.lock().unwrap(), vec![Value::string("typed")]);
    }

    /// Refusing a line is rewriting it to a failing one, whose status reaches
    /// `post-exec` through the ordinary run.
    #[test]
    fn pre_exec_refuses_by_rewriting_to_a_failing_line() {
        let refusal = "fail [status: 4, message: 'refused']";
        let (t, host, sink) = dressed(&[
            (
                "p",
                HookEvent::PreExec,
                &format!("{{ |_ev| return \"{refusal}\" }}"),
            ),
            (
                "p",
                HookEvent::PostExec,
                "{ |ev| record $ev[src] $ev[status] }",
            ),
        ]);
        run("record never", &t, &host);
        assert_eq!(
            *sink.lock().unwrap(),
            vec![Value::string(refusal), Value::Int(4)]
        );
    }

    /// `chpwd` observes the session's directory at the line boundary: a `cd`
    /// that a `within [dir: …]` undoes fires nothing, and a plain one fires
    /// once, with the line's `old` and `new`.
    #[test]
    fn chpwd_fires_only_when_the_line_moves_the_session() {
        let root = tempfile::tempdir().unwrap();
        // The session reports the resolved path, not a Windows temp 8.3 alias.
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let root = root_path.as_path();
        std::fs::create_dir_all(root.join("d").join("x")).unwrap();
        let (t, host, sink) =
            dressed(&[("p", HookEvent::Chpwd, "{ |ev| record $ev[old] $ev[new] }")]);
        run(&format!("cd '{}'", ral_core::path::shown(root)), &t, &host);
        sink.lock().unwrap().clear();

        run(
            &format!(
                "within [dir: '{}'] {{ cd x }}",
                ral_core::path::shown(root.join("d"))
            ),
            &t,
            &host,
        );
        assert!(sink.lock().unwrap().is_empty(), "the session never moved");

        run("cd d", &t, &host);
        assert_eq!(
            *sink.lock().unwrap(),
            vec![
                Value::string(ral_core::path::shown(root)),
                Value::string(ral_core::path::shown(root.join("d"))),
            ]
        );
    }
}
