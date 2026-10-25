//! The REPL's [`Host`]: it answers the engine's `` `repl-editor ``,
//! `` `repl-plugin `` and `` `repl-clipboard `` enquiries, renders what the engine surfaces, and is
//! the one place every plugin hook's dispatch goes through.

use clipboard::{Clipboard, Copied};
use ral_core::carrier::{Host, Severed, Transport, dispatch_to_report};
use ral_core::data::FOValue;
use ral_core::data::datum::{Datum as _, untag};
use ral_core::process::CancelCause;
use ral_core::protocol::probe::{WorkerRow, WorkerState};
use ral_core::protocol::{Ending, EnquiryError, Program, Report, Run};
use ral_core::source::SourceOrigin;
use ral_core::sync::LockExt as _;
use ral_core::text::plural;
use ral_core::types::{DeferredSink, HookName};
use ral_core::{Captured, RequestedTerminalAccess, Value};
use ral_core::{errln, outln};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::enquiry::{ClipboardReq, Enquiry};
use super::plugin::PluginRuntime;
use super::plugin::editor::PluginContext;

/// rustyline's printer: a line lands above the prompt, not over it.
pub(super) type Printer = Box<dyn rustyline::ExternalPrinter + Send>;

pub(super) struct ReplHost {
    pub(super) runtime: Arc<Mutex<PluginRuntime>>,
    /// The editor context of the dispatch in flight, if it installed one.
    editor: Mutex<Option<PluginContext>>,
    printer: Mutex<Option<Printer>>,
    /// The session prompt's latest fault, so a broken prompt says so once.
    prompt_fault: Mutex<Option<String>>,
    clipboard: Mutex<Clipboard>,
}

/// How a hook run ended.  An `exit` from a hook is ignored.  A fault is
/// rendered by the engine and labelled with the hook's name; a stop is
/// reported like a fault but is nobody's fault.
pub(super) enum Outcome {
    Settled(FOValue),
    Exited,
    Faulted(String),
    Walled(String),
    Stopped(String),
}

/// What one hook dispatch came to.
pub(super) struct HookResult {
    pub(super) outcome: Outcome,
    pub(super) captured: Option<Captured>,
    pub(super) ctx: Option<PluginContext>,
}

/// A run of a registered hook: its registered policy decides capture,
/// terminal authority, and whether it runs aside.
pub(super) fn hook_run(program: Program, wall: Option<Duration>) -> Run {
    Run {
        terminal: RequestedTerminalAccess::Denied,
        wall,
        ..Run::foreground(program, SourceOrigin::named("<hook>"))
    }
}

impl ReplHost {
    pub(super) fn new(runtime: Arc<Mutex<PluginRuntime>>) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            editor: Mutex::default(),
            printer: Mutex::default(),
            prompt_fault: Mutex::default(),
            clipboard: Mutex::new(Clipboard::new(
                ral_core::host::var("SSH_CONNECTION").is_some(),
            )),
        })
    }

    /// Record the session prompt's latest `fault`: the one to print, if it
    /// is not the one already printed.
    pub(super) fn prompt_fault(&self, fault: Option<String>) -> Option<String> {
        let mut last = self.prompt_fault.lock_ignore_poison();
        if *last == fault {
            return None;
        }
        last.clone_from(&fault);
        fault
    }

    pub(super) fn set_printer(&self, printer: Option<Printer>) {
        *self.printer.lock_ignore_poison() = printer;
    }

    /// Dispatch `run` with `ctx` installed as its editor context, and take
    /// the context back.
    pub(super) fn dispatch(
        self: &Arc<Self>,
        t: &dyn Transport,
        run: Run,
        ctx: Option<PluginContext>,
    ) -> (Result<Report, Severed>, Option<PluginContext>) {
        *self.editor.lock_ignore_poison() = ctx;
        let report = dispatch_to_report(t, run, self.clone());
        (report, self.editor.lock_ignore_poison().take())
    }

    pub(super) fn run_hook(
        self: &Arc<Self>,
        t: &dyn Transport,
        name: HookName,
        args: Vec<FOValue>,
        wall: Option<Duration>,
        ctx: Option<PluginContext>,
    ) -> HookResult {
        let (report, ctx) = self.dispatch(t, hook_run(Program::Hook { name, args }, wall), ctx);
        let (mut outcome, captured) = match report {
            Err(severed) => (Outcome::Faulted(format!("ral: {severed}")), None),
            Ok(Report::Static { rendered, .. } | Report::Faulted { rendered, .. }) => {
                (Outcome::Faulted(rendered), None)
            }
            Ok(Report::Ran {
                ending, captured, ..
            }) => (
                match ending {
                    Ending::Settled { value, .. } => Outcome::Settled(value),
                    Ending::Exited(_) => Outcome::Exited,
                    Ending::Stopped {
                        rendered,
                        cause: CancelCause::TimedOut,
                        ..
                    } => Outcome::Walled(rendered),
                    Ending::Stopped { rendered, .. } => Outcome::Stopped(rendered),
                    Ending::Raised { rendered, .. } | Ending::Unreturnable { rendered, .. } => {
                        Outcome::Faulted(rendered)
                    }
                },
                captured,
            ),
        };
        if let Outcome::Faulted(f) | Outcome::Walled(f) | Outcome::Stopped(f) = &mut outcome {
            f.truncate(f.trim_end().len());
        }
        HookResult {
            outcome,
            captured,
            ctx,
        }
    }

    fn line(&self, line: &str) {
        if let Some(p) = self.printer.lock_ignore_poison().as_mut()
            && p.print(format!("{line}\n")).is_ok()
        {
            return;
        }
        outln!("{line}");
    }

    fn render(&self, v: &FOValue) {
        if let Some(line) = crate::surface::watch_line(v) {
            return self.line(&line);
        }
        match untag(v) {
            Some(("notice", Some(payload))) => self.line(&format!(
                "{}note: {}{}",
                ral_core::io::ansi::DIM,
                Value::from(payload.clone()),
                ral_core::io::ansi::RESET
            )),
            _ => {
                if let Some(note) = crate::surface::dropped(v) {
                    errln!("{note}");
                }
            }
        }
    }
}

impl Host for ReplHost {
    fn surface(&self, val: &FOValue) {
        self.render(val);
    }

    fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
        let refuse = |message: String| EnquiryError { message, status: 1 };
        match Enquiry::decode(&req).map_err(refuse)? {
            Enquiry::Editor(op) => self
                .editor
                .lock_ignore_poison()
                .as_mut()
                .map(|ctx| ctx.apply(op))
                .ok_or_else(|| {
                    refuse("editor op: no plugin context (not inside a plugin handler)".into())
                }),
            Enquiry::Plugin(note) => super::plugin::lock(&self.runtime)
                .note(note)
                .map(|()| FOValue::Unit)
                .map_err(refuse),
            Enquiry::Clipboard(ClipboardReq { text, osc52 }) => {
                let whole = matches!(
                    self.clipboard.lock_ignore_poison().copy(&text, osc52),
                    Ok(Copied::Whole)
                );
                Ok(whole.encode())
            }
        }
    }
}

impl DeferredSink for ReplHost {
    fn deliver(&self, batch: Vec<FOValue>) {
        for v in &batch {
            self.render(v);
        }
    }
}

/// Compose the shell-exit notice: one compact line naming every worker still
/// running when the REPL tears down, or `None` when none are — POSIX's "you
/// have stopped jobs" register for the worker population. It announces the
/// sweep the engine performs as it drops, never performs one itself.
pub(crate) fn teardown_notice(workers: &[WorkerRow]) -> Option<String> {
    let running: Vec<String> = workers
        .iter()
        .filter(|row| matches!(row.state, WorkerState::Running | WorkerState::Stopping))
        .map(|row| format!("#{} ({})", row.id, row.title))
        .collect();
    if running.is_empty() {
        return None;
    }
    Some(format!(
        "ral: taking down {} still running: {}",
        plural(running.len(), "worker"),
        running.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_state(id: u64, title: &str, state: WorkerState) -> WorkerRow {
        let settled = matches!(state, WorkerState::Ready(_)).then_some(0);
        WorkerRow {
            id,
            title: title.to_string(),
            owner: ral_core::types::Owner::Session,
            state,
            started: 0,
            elapsed: ral_core::data::Finite::from(0),
            settled,
            retention_left: None,
        }
    }

    fn row(id: u64, title: &str, running: bool) -> WorkerRow {
        row_state(
            id,
            title,
            if running {
                WorkerState::Running
            } else {
                WorkerState::Ready(ral_core::types::Done::Ok)
            },
        )
    }

    /// `teardown_notice` names every still-live (`Running` or `Stopping`)
    /// worker in one line and is `None` when the table holds none.
    #[test]
    fn teardown_notice_names_running_workers_only() {
        assert_eq!(
            teardown_notice(&[]),
            None,
            "nothing running, nothing to announce"
        );
        assert_eq!(
            teardown_notice(&[row(1, "done", false)]),
            None,
            "a ready-but-unclaimed worker is nothing to take down"
        );

        let mixed = [
            row(2, "still going", true),
            row_state(9, "serving", WorkerState::Stopping),
            row(1, "done", false),
        ];
        let notice = teardown_notice(&mixed).expect("live workers must be named");
        assert!(notice.contains("2 workers still running"), "got: {notice}");
        assert!(notice.contains("#2 (still going)"), "got: {notice}");
        assert!(notice.contains("#9 (serving)"), "got: {notice}");
        assert!(!notice.contains("#1"), "the ready worker is not named");
    }

    /// A prompt fault prints when it changes: once while it repeats, again
    /// when it differs, and again after a render that did not fail.
    #[test]
    fn a_prompt_fault_prints_when_it_changes() {
        let host = ReplHost::new(Arc::default());
        let boom = || Some("prompt: boom".to_string());
        assert_eq!(host.prompt_fault(boom()), boom());
        assert_eq!(host.prompt_fault(boom()), None);
        assert_eq!(
            host.prompt_fault(Some("prompt: bang".into())).as_deref(),
            Some("prompt: bang")
        );
        assert_eq!(host.prompt_fault(None), None);
        assert_eq!(host.prompt_fault(boom()), boom());
    }

    /// Outside an editor context the host refuses in the words the builtins used
    /// before the context moved host-side.
    #[test]
    fn an_editor_op_outside_a_handler_is_refused() {
        let host = ReplHost::new(Arc::default());
        let req = Enquiry::Editor(super::super::enquiry::EditorOp::Get).encode();
        let err = host.enquire(req).expect_err("no context installed");
        assert!(err.message.contains("no plugin context"), "{}", err.message);
    }
}
