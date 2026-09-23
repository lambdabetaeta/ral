//! The REPL's [`Host`]: it answers the engine's `` `repl-editor `` and
//! `` `repl-plugin `` enquiries, renders what the engine surfaces, and is
//! the one door every plugin hook's dispatch goes through.

use ral_core::protocol::reading::WorkerRow;
use ral_core::protocol::{
    Ending, EnquiryError, Host, Program, Report, Run, Severed, Transport, dispatch_to_report,
};
use ral_core::serial::FOValue;
use ral_core::serial::datum::{Datum as _, untag};
use ral_core::sync::LockExt as _;
use ral_core::types::{DeferredSink, GrantStack, HookName};
use ral_core::{Captured, RequestedTerminalAccess, RunIo, RunStdin, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::enquiry::Enquiry;
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
}

/// What one hook dispatch came to.  An `exit` from a hook is ignored, so it
/// leaves both `value` and `fault` empty.
#[derive(Default)]
pub(super) struct HookResult {
    pub(super) value: Option<FOValue>,
    pub(super) captured: Option<Captured>,
    /// The fault, rendered by the engine and labelled with the hook's name.
    pub(super) fault: Option<String>,
    pub(super) walled: bool,
    pub(super) ctx: Option<PluginContext>,
}

/// A run of a registered hook: its registered policy decides capture,
/// terminal authority, and whether it runs aside.
pub(super) fn hook_run(program: Program, wall: Option<Duration>) -> Run {
    Run {
        program,
        script_name: "<hook>".into(),
        caps: GrantStack::root(),
        wall,
        deferred_lease: None,
        worker_cap: None,
        io: RunIo::Inherit,
        terminal: RequestedTerminalAccess::Denied,
        stdin: RunStdin::Inherit,
        trail: None,
    }
}

impl ReplHost {
    pub(super) fn new(runtime: Arc<Mutex<PluginRuntime>>) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            editor: Mutex::default(),
            printer: Mutex::default(),
            prompt_fault: Mutex::default(),
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
        let mut out = HookResult {
            ctx,
            ..HookResult::default()
        };
        let fault = match report {
            Err(severed) => format!("ral: {severed}"),
            Ok(Report::Static { rendered, .. }) => rendered,
            Ok(Report::Ran {
                ending, captured, ..
            }) => {
                out.captured = captured;
                match ending {
                    Ending::Settled { value, .. } => {
                        out.value = Some(value);
                        return out;
                    }
                    Ending::Exited(_) => return out,
                    Ending::Walled { rendered, .. } => {
                        out.walled = true;
                        rendered
                    }
                    Ending::Raised { rendered, .. } | Ending::Unreturnable { rendered, .. } => {
                        rendered
                    }
                }
            }
        };
        out.fault = Some(fault.trim_end().to_string());
        out
    }

    fn line(&self, line: &str) {
        if let Some(p) = self.printer.lock_ignore_poison().as_mut()
            && p.print(format!("{line}\n")).is_ok()
        {
            return;
        }
        println!("{line}");
    }

    fn render(&self, v: &FOValue) {
        if let Some(line) = crate::surface::watch_line(v) {
            return self.line(&line);
        }
        match untag(v) {
            Some(("notice", Some(payload))) => self.line(&format!(
                "{}note: {}{}",
                ral_core::ansi::DIM,
                Value::from(payload.clone()),
                ral_core::ansi::RESET
            )),
            _ => {
                if let Some(note) = crate::surface::dropped(v) {
                    eprintln!("{note}");
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
        .filter(|row| row.running)
        .map(|row| format!("[w{}] {}", row.id, row.cmd))
        .collect();
    if running.is_empty() {
        return None;
    }
    Some(format!(
        "ral: taking down {} still-running worker{}: {}",
        running.len(),
        if running.len() == 1 { "" } else { "s" },
        running.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, cmd: &str, running: bool) -> WorkerRow {
        WorkerRow {
            id,
            cmd: cmd.to_string(),
            class: ral_core::types::LeaseClass::Worker,
            running,
            up_secs: 0,
            idle_secs: 0,
            settled_epoch: None,
        }
    }

    /// `teardown_notice` names every still-running worker in one line and is
    /// `None` when the table holds none.
    #[test]
    fn teardown_notice_names_running_workers_only() {
        assert_eq!(
            teardown_notice(&[]),
            None,
            "nothing running, nothing to announce"
        );
        assert_eq!(
            teardown_notice(&[row(1, "spawn { done }", false)]),
            None,
            "a settled-but-unclaimed worker is nothing to take down"
        );

        let mixed = [
            row(2, "spawn { still_going }", true),
            row(9, "service { daemon }", true),
            row(1, "spawn { done }", false),
        ];
        let notice = teardown_notice(&mixed).expect("two running workers must be named");
        assert!(notice.contains("2 still-running workers"), "got: {notice}");
        assert!(
            notice.contains("[w2] spawn { still_going }"),
            "got: {notice}"
        );
        assert!(notice.contains("[w9] service { daemon }"), "got: {notice}");
        assert!(!notice.contains("[w1]"), "the settled worker is not named");
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

    /// Outside an editor context the host refuses in the words the doors used
    /// before the context moved host-side.
    #[test]
    fn an_editor_op_outside_a_handler_is_refused() {
        let host = ReplHost::new(Arc::default());
        let req = Enquiry::Editor(super::super::enquiry::EditorOp::Get).encode();
        let err = host.enquire(req).expect_err("no context installed");
        assert!(err.message.contains("no plugin context"), "{}", err.message);
    }
}
