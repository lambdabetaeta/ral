//! The engine's answers: each [`Probe`] read off the shell, at a run boundary.
//!
//! Every answer is data, rendered here and never a live handle.

use super::Engine;
use crate::first_order::FOValue;
use crate::first_order::datum::Datum;
use crate::protocol::probe::{BindingRow, CompletionNames, HandleRow, Probe, WorkerRow};
use crate::types::{Shell, Value, WorkerEntry};

mod fs;

impl Engine {
    /// Read one probe, at a run boundary.
    pub(crate) fn probe(&self, probe: &Probe) -> FOValue {
        answer(&self.shell, probe)
    }
}

/// Answer one probe against `shell`.
pub(crate) fn answer(shell: &Shell, probe: &Probe) -> FOValue {
    match probe {
        Probe::BindingCount => shell.binding_count().encode(),
        Probe::LeasedBindingCount => shell.leased_binding_count().encode(),
        Probe::LargestBindingBytes => shell.largest_binding_shallow_size().encode(),
        Probe::EnvVar(name) => shell.env_var(name).encode(),
        Probe::Cwd => shell.cwd().display().to_string().encode(),
        Probe::Home => shell.context.home().encode(),
        Probe::BuiltinNames => shell
            .builtin_names()
            .map(str::to_string)
            .collect::<Vec<_>>()
            .encode(),
        Probe::PathBytes(path) => fs::tree_bytes(&shell.cwd().join(path)).encode(),
        Probe::Workers => shell
            .workers()
            .iter()
            .map(|entry| worker_row(shell, entry))
            .collect::<Vec<_>>()
            .encode(),
        Probe::SessionEnded => shell
            .durable_root()
            .as_scope()
            .cause()
            .map(|cause| i32::from(cause.code()))
            .encode(),
        Probe::CompletionNames => completion_names(shell).encode(),
        Probe::Bindings => binding_rows(shell).encode(),
        Probe::PathEntries(dir) => fs::entries(&shell.cwd().join(dir)).encode(),
        #[cfg(feature = "test-util")]
        Probe::WorkerCount => shell.worker_count().encode(),
        #[cfg(feature = "test-util")]
        Probe::GrantDepth => shell.grant_depth().encode(),
    }
}

fn worker_row(shell: &Shell, entry: &WorkerEntry) -> WorkerRow {
    WorkerRow {
        id: entry.id.0,
        cmd: entry.cmd.clone(),
        class: entry.class,
        running: entry.handle.is_running(),
        up_secs: entry.started.elapsed().unwrap_or_default().as_secs(),
        idle_secs: entry.handle.last_observed().elapsed().as_secs(),
        retention_left: shell.worker_retention_left(entry),
    }
}

/// Every binding in scope, a shadowed name once.
fn binding_rows(shell: &Shell) -> Vec<BindingRow> {
    shell
        .env
        .fold_union(&shell.sig, |b| {
            let handle = match &b.value {
                Value::Handle(h) => Some(HandleRow {
                    state: h.state(),
                    cmd: h.cmd.clone(),
                }),
                _ => None,
            };
            (
                b.scheme.as_deref().map(ToString::to_string),
                preview(&b.value),
                handle,
            )
        })
        .into_iter()
        .map(|(name, (scheme, preview, handle))| BindingRow {
            name,
            scheme,
            preview,
            handle,
        })
        .collect()
}

fn preview(v: &Value) -> String {
    const CAP: usize = 40;
    let s = v.to_string().replace('\n', " ");
    if s.chars().count() <= CAP {
        return s;
    }
    s.chars().take(CAP - 1).chain(['…']).collect()
}

fn completion_names(shell: &Shell) -> CompletionNames {
    let sorted = |mut names: Vec<String>| {
        names.sort();
        names.dedup();
        names
    };
    CompletionNames {
        bindings: sorted(
            shell
                .env
                .fold_union(&shell.sig, |_| ())
                .into_iter()
                .map(|(n, ())| n)
                .collect(),
        ),
        handlers: sorted(shell.handler_names().map(str::to_string).collect()),
    }
}
