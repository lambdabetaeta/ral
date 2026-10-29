//! In-process ral evaluation against a persistent `Shell`.
//!
//! Each tool call is a top-level run under its engine's session layers, with
//! stdout and stderr captured into buffers rather than streamed: the model
//! reads them back from history, the user sees only what the rail renders.
//! There is no source-level `grant { … }` around the model's body — the
//! boundary is the session's host-installed layers, not surface syntax the
//! model could evade.

pub mod builtins;
pub(crate) mod clip;
pub(crate) mod report;
pub mod surface;

use crate::bus::{AgentAddress, Input};
use base64::Engine;
use ral_core::Shell;
use ral_core::Value as RalValue;
use ral_core::data::FOValue;
use ral_core::shell::runtime::{DeferredSink, TitlePolicy};
use std::sync::Arc;
use std::time::Duration;

/// Admission cap on concurrently *running* workers per agent, enforced by core
/// at the spawn. A settled entry lingering under retention holds no slot.
pub(crate) const LIVE_WORKER_CAP: usize = 64;

/// Birth budget on `detach` per session shell.
///
/// Deliberately not [`LIVE_WORKER_CAP`]: a detach occupies no slot, so a cap
/// counting live work cannot bound it, and it needs a bound of its own
/// precisely because nothing later reclaims it.  Reset by `/clear`, which
/// reboots the shell.
#[cfg(unix)]
pub(crate) const DETACH_BIRTH_BUDGET: u64 = 16;

/// Retention bound, in ral calls, on a settled worker's unclaimed result,
/// counted from the per-call sweep in `Agent::ral` that first
/// observes it settled.
pub(crate) const SETTLED_WORKER_RETENTION: u64 = 256;

/// Idle bound, in committed ral calls, on the binding-lease ledger armed on
/// every agent shell: a top-level name unused this long is pruned at the next
/// ready boundary.  Shares its figure with [`SETTLED_WORKER_RETENTION`] — one
/// ral-call clock, two ledgers reading it for their own idle policy.
pub(crate) const BINDING_IDLE_CALLS: u64 = 256;

/// Soft threshold on a session-scope install's `Value::shallow_size`: meeting
/// it queues a `LargeBindingNotice`, a nudge toward a file path over captured
/// bytes, never an eviction.
pub(crate) const LARGE_BINDING_BYTES: u64 = 1024 * 1024;

pub(crate) fn arm_session_ledgers(shell: &mut Shell) {
    shell.arm_binding_lease(ral_core::shell::runtime::BindingLease {
        idle_calls: BINDING_IDLE_CALLS,
        large_binding_bytes: LARGE_BINDING_BYTES,
    });
    shell.arm_worker_retention(SETTLED_WORKER_RETENTION);
}

/// The prelude baked into this binary at build time by `build.rs`.
pub static PRELUDE: ral_core::shell::boot::BakedPrelude = ral_core::baked_prelude!();

/// A successful tool run, kept in named pieces so `agent::digest::render` can
/// clip each section against its own cap and one oversized stream cannot crowd
/// out the others.
pub struct ToolResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: Option<String>,
    pub exit: i32,
}

/// One mirrored pin.  The bus and scrollback carry `Forensic::Pin` and
/// `Transient::Pin`; this exists only inside the agent's session mirror.
#[derive(Clone, Debug)]
pub struct PinDigest {
    pub(crate) card: crate::card::Card,
}

impl PinDigest {
    pub(crate) fn new(card: crate::card::Card) -> Self {
        Self { card }
    }
}

/// Deferred worker batches carry the engine generation that produced them.
struct SurfacePoster {
    address: AgentAddress,
    generation: u64,
}

impl DeferredSink for SurfacePoster {
    fn deliver(&self, values: Vec<FOValue>) {
        let _ = self.address.post(Input::Surface {
            generation: self.generation,
            values,
        });
    }
}

/// Workers retain their engine's generation across later dispatches.
pub(crate) fn deferred_sink(address: &AgentAddress, generation: u64) -> Arc<dyn DeferredSink> {
    Arc::new(SurfacePoster {
        address: address.clone(),
        generation,
    })
}

/// Evaluate `cmd` against `transport`, capturing stdout and
/// stderr.  Everything crosses the engine protocol: a `Source` `Run` out, a
/// stream of surface events drained to the bus, one terminal `Report` back.
///
/// `source` names the call's text wherever a position is shown — a
/// diagnostic, a trail site, a worker spawned from it.  `host` is the host's
/// side of this run — the test harness's bare `IdentityTransport` hands a
/// pin-less, desk-less [`surface::SurfaceApplier`], every real
/// caller a [`crate::agent::desk::RunHost`].
pub(crate) fn run_shell(
    transport: &dyn ral_core::carrier::Transport,
    source: &str,
    cmd: &str,
    timeout_secs: u64,
    host: Arc<dyn ral_core::carrier::Host>,
    within: Option<&ral_core::process::Scope>,
) -> Result<ral_core::protocol::Report, ral_core::carrier::Severed> {
    // Trace-only timing.
    #[cfg(debug_assertions)]
    let tool_start = std::time::Instant::now();

    use ral_core::protocol::Run;
    use ral_core::source::SourceOrigin;

    let run = Run {
        wall: Some(Duration::from_secs(timeout_secs)),
        cleanup_grace: Duration::from_secs(2),
        worker_cap: Some(LIVE_WORKER_CAP),
        worker_title: TitlePolicy::Required,
        ..Run::captured(cmd, SourceOrigin::named(source))
    };

    let report = ral_core::carrier::dispatch_to_report(transport, run, host, within);
    ral_core::dbg_trace!("shell", "eval in {:?}", tool_start.elapsed());
    report
}

/// Print settings for the `VALUE` section: structured values print in ral
/// surface syntax rather than detouring through JSON, so variants stay variants
/// and records stay records.
///
/// `max_string: 0` — no per-string cap here, where the REPL keeps one. A
/// terminal's scarcity is the row, and 72 characters is a row; this reader's
/// scarcity is context, which `max_bytes` already bounds in its own unit, so a
/// second cap in a second unit buys only damage. And the damage is not cosmetic:
/// a nested string here is usually a payload whose *text is its identity* — a
/// `view-hash` row's line, a `grep-files` hit — which `edit-hash` and
/// `edit-replace` both match verbatim, so a line rendered in part is a line that
/// cannot be edited. Spending the byte budget instead costs whole rows, and each
/// container says how many it dropped.
const VALUE_PRINT_PARAMS: ral_core::shell::builtins::PrintParams =
    ral_core::shell::builtins::PrintParams {
        max_width: 120,
        max_string: 0,
        max_depth: 3,
        min_quote_hashes: 1,
        quote_bytes: true,
        max_bytes: 16 * 1024,
    };

/// Render a first-order value as the text the `VALUE` section carries.  A top-level
/// string or byte string is a payload and passes through raw, so file windows,
/// markdown reports, and captured byte text keep their exact lines.
pub(crate) fn ral_value_to_text(value: &FOValue) -> Option<String> {
    match value {
        FOValue::Unit => None,
        FOValue::String { value } => Some(value.clone()),
        FOValue::Bytes { value } => Some(String::from_utf8_lossy(value).into_owned()),
        other => Some(ral_core::shell::builtins::pretty_print(
            &RalValue::from(other.clone()),
            0,
            &VALUE_PRINT_PARAMS,
        )),
    }
}

/// Project a `reply`'s first-order payload to the JSON a user-facing edge (the
/// headless `result`) reads — deliberately **not** [`FOValue`]'s own `serde`
/// impl, which is the transport encoding (externally tagged, floats as
/// IEEE-754 bits) and would hand the user a `{"string":{"value":…}}` wrapper
/// where a bare JSON string was promised.  JSON has no byte type, so bytes cross
/// as base64.
pub(crate) fn user_json(v: &FOValue) -> serde_json::Value {
    match v {
        FOValue::Unit => serde_json::Value::Null,
        FOValue::Bool { value } => serde_json::Value::Bool(*value),
        FOValue::Int { value } => serde_json::Value::Number((*value).into()),
        FOValue::Float { value } => value.get().into(),
        FOValue::String { value } => serde_json::Value::String(value.clone()),
        FOValue::Bytes { value } => {
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(value))
        }
        FOValue::List { items } => serde_json::Value::Array(items.iter().map(user_json).collect()),
        FOValue::Map { entries } => serde_json::Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), user_json(v)))
                .collect(),
        ),
        FOValue::Variant {
            label,
            payload: Some(p),
        } => {
            let mut m = serde_json::Map::new();
            m.insert(label.clone(), user_json(p));
            serde_json::Value::Object(m)
        }
        FOValue::Variant {
            label,
            payload: None,
        } => serde_json::Value::String(label.clone()),
        #[allow(
            clippy::uninhabited_references,
            reason = "NoExt is uninhabited, so this arm never actually runs; the dereference \
                      exhaustiveness needs is never performed at runtime"
        )]
        FOValue::Ext(x) => match *x {},
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
