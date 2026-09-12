//! Captured builtin entries for plugin-lifecycle commands.
//!
//! [`build`] returns the two entries installed into the session builtin
//! table at REPL boot.  The closures receive the `args` slice verbatim,
//! with no handler argv packing.

use ral_core::diagnostic;
use ral_core::sync::LockExt as _;
use ral_core::typecheck::builtins::scheme;
use ral_core::types::{
    Break, BuiltinBody, BuiltinEntry, HandleState, Mooring, Resident, WorkerEntry,
};
use ral_core::{Shell, Value};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use super::plugin::PluginRuntime;

/// Build the two session builtin entries, capturing `runtime` by
/// `Arc<Mutex<…>>` so each closure owns its share of the long-lived state.
pub fn build(runtime: Arc<Mutex<PluginRuntime>>) -> Arc<[BuiltinEntry]> {
    vec![
        build_load_plugin(runtime.clone()),
        build_unload_plugin(runtime),
    ]
    .into()
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// The plugin name a `load-plugin`/`unload-plugin` invocation targets.
/// Both verbs are `STRING_TO_UNIT`, so type-checking guarantees the one
/// argument; an empty slice can only be an internal error, reported via
/// `cmd_error` rather than a raised fault.
fn plugin_name_arg(args: &[Value]) -> Option<String> {
    args.first().map(std::string::ToString::to_string)
}

/// Compose the shell-exit notice: one compact line naming every worker handle
/// still `Running` when the REPL tears down, or `None` when none are — POSIX's
/// "you have stopped jobs" register for the worker population. It announces the
/// sweep the session's shell performs as it drops, never performs one itself.
pub(crate) fn teardown_notice(workers: &[WorkerEntry]) -> Option<String> {
    let running: Vec<String> = workers
        .iter()
        .filter(|entry| *entry.handle.state.lock_ignore_poison() == HandleState::Running)
        .map(|entry| format!("[{}] {}", entry.designator(), entry.cmd))
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

// ── load-plugin ───────────────────────────────────────────────────────────────

fn build_load_plugin(runtime: Arc<Mutex<PluginRuntime>>) -> BuiltinEntry {
    BuiltinEntry::new(
        Cow::Borrowed("load-plugin"),
        scheme::string_to_unit,
        "load-plugin <name>  — load a REPL plugin by name or path.",
        BuiltinBody::Captured(Arc::new(
            move |args, mooring: &Mooring, shell: &mut Shell| {
                let Some(name) = plugin_name_arg(args) else {
                    diagnostic::cmd_error("load-plugin", "missing plugin name");
                    return Ok(Value::Unit);
                };
                // No options: `load-plugin` takes a name alone, so a plugin
                // loaded through it stands on its own defaults.
                if let Err(Break::Error(e)) = super::plugin::load::load_plugin(
                    &name,
                    &ral_core::types::Map::new(),
                    mooring,
                    shell,
                    &runtime,
                ) {
                    diagnostic::cmd_error("load-plugin", &e.message);
                }
                Ok(Value::Unit)
            },
        )),
    )
}

// ── unload-plugin ─────────────────────────────────────────────────────────────

fn build_unload_plugin(runtime: Arc<Mutex<PluginRuntime>>) -> BuiltinEntry {
    BuiltinEntry::new(
        Cow::Borrowed("unload-plugin"),
        scheme::string_to_unit,
        "unload-plugin <name>  — unload a previously loaded REPL plugin.",
        BuiltinBody::Captured(Arc::new(
            move |args, _mooring: &Mooring, shell: &mut Shell| {
                let Some(name) = plugin_name_arg(args) else {
                    diagnostic::cmd_error("unload-plugin", "missing plugin name");
                    return Ok(Value::Unit);
                };
                if let Err(e) = super::plugin::load::unload_plugin(&name, shell, &runtime) {
                    diagnostic::cmd_error("unload-plugin", &e.message);
                }
                Ok(Value::Unit)
            },
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal registered-worker fixture, `running` toggling
    /// [`HandleState::Running`] vs [`HandleState::Completed`] — enough to
    /// exercise [`teardown_notice`] without a real `spawn`.  Building
    /// `HandleInner` field by field is legitimate here: core's representation
    /// is sealed against exarch, not against a sibling crate, and core's own
    /// concurrency tests construct it the same way.
    fn fake_worker(id: u64, cmd: &str, running: bool) -> WorkerEntry {
        let state = if running {
            HandleState::Running
        } else {
            HandleState::Completed
        };
        WorkerEntry {
            id: ral_core::types::WorkerId(id),
            cmd: cmd.to_string(),
            started: std::time::SystemTime::now(),
            class: ral_core::types::LeaseClass::Worker,
            settled_epoch: None,
            handle: ral_core::types::HandleInner {
                result: Arc::new(Mutex::new(None)),
                cached: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(state)),
                stdout_buf: ral_core::io::ByteBuffer::default(),
                stderr_buf: ral_core::io::ByteBuffer::default(),
                surface_buf: Arc::new(Mutex::new(Vec::new())),
                joined: Arc::new(Mutex::new(false)),
                last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
                cmd: cmd.to_string(),
                cancel: ral_core::process::CancelScope::default(),
            },
        }
    }

    /// `teardown_notice` names every still-running worker in one line and is
    /// `None` when the registry holds none.
    #[test]
    fn teardown_notice_names_running_workers_only() {
        assert_eq!(
            teardown_notice(&[]),
            None,
            "nothing running, nothing to announce"
        );

        let settled_only = vec![fake_worker(1, "spawn { done }", false)];
        assert_eq!(
            teardown_notice(&settled_only),
            None,
            "a settled-but-unclaimed worker is nothing to take down"
        );

        let mixed = vec![
            fake_worker(2, "spawn { still_going }", true),
            fake_worker(9, "service { daemon }", true),
            fake_worker(1, "spawn { done }", false),
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
}
