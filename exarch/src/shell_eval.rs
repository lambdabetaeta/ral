//! In-process ral evaluation against a persistent `Shell`.
//!
//! Each tool call is a top-level run under a pushed capabilities frame, with
//! stdout and stderr captured into buffers rather than streamed: the model
//! reads them back from history, the user sees only what the rail renders.
//! There is no source-level `grant { … }` around the model's body — the
//! boundary is the run door plus the pushed frame, not surface syntax the
//! model could evade.

pub mod builtins;
pub(crate) mod report;

use crate::bus::{Emitter, Stamp, Stamped};
use crate::card::{Change, landing, value_to_card, value_to_done, value_to_edit};
use crate::record::AgentId;
use base64::Engine;
use ral_core::Shell;
use ral_core::Value as RalValue;
use ral_core::first_order::FOValue;
use ral_core::types::{DeferredSink, Observation};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Idle bound of the lease armed on every detached `spawn` worker, renewed by
/// any `poll`/`await`/`race`: an abandoned worker is reaped, a babysat one
/// lives.  `pub(crate)` so the `/resources` fold reads the same constant its
/// lease rows describe.
pub(crate) const DETACHED_WORKER_CEILING: Duration = Duration::from_hours(1);

/// Absolute backstop of the same lease, measured from spawn: observation
/// renews idleness, never age.
pub(crate) const DETACHED_WORKER_BACKSTOP: Duration = Duration::from_hours(24);

/// Admission cap on concurrently *running* workers per agent, enforced by core
/// at the spawn door.  A durable service holds a seat; a settled entry
/// lingering under retention does not.
pub(crate) const LIVE_WORKER_CAP: usize = 64;

/// Birth budget on `detach` per session shell.
///
/// Deliberately not [`LIVE_WORKER_CAP`]: a detach occupies no seat, so a cap
/// counting live work cannot bound it, and it needs a bound of its own
/// precisely because nothing later reclaims it.  Reset by `/clear`, which
/// reboots the shell.
pub const DETACH_BIRTH_BUDGET: u64 = 16;

/// Retention bound, in ral calls, on a settled worker's unclaimed result,
/// counted from the per-call epoch sweep in `Avatar::ral` that first
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
    shell.arm_binding_lease(ral_core::types::BindingLease {
        idle_calls: BINDING_IDLE_CALLS,
        large_binding_bytes: LARGE_BINDING_BYTES,
    });
    shell.arm_worker_retention(SETTLED_WORKER_RETENTION);
}

/// The prelude baked into this binary at build time by `build.rs`.
pub static PRELUDE: ral_core::boot::BakedPrelude = ral_core::baked_prelude!();

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

/// A shared, session-owned register of pinned-state digests, written by the
/// desk's `exarch-pins` family and read back by its `` `read ``/`` `list ``
/// and by the boundary nudge.
pub type PinDigests = Arc<Mutex<std::collections::BTreeMap<String, PinDigest>>>;

/// What the record absorbs: the shapes the `surface` channel carries, plus
/// the register writes `exarch-pins` makes — closed and named rather than
/// borrowed from the bus's vocabulary.
///
/// `Surface` carries the fact a card *is* (`Card`, `Pin`) rather than one a
/// printer merely wants a copy of — an observation, a change, a notice, and
/// a done keep only their structured value, so a printer's own mark tree is
/// built once, by whoever renders, not eagerly here and thrown away by
/// whoever records.
pub enum Surface {
    Observation(Box<Observation>),
    /// What a write or an edit did to a file.
    Change(crate::card::Change),
    Card(crate::card::Card),
    Notice(ral_core::types::Notice),
    Done {
        cmd: String,
        outcome: crate::card::DoneOutcome,
    },
    Pin {
        key: String,
        card: crate::card::Card,
    },
    Unpin {
        key: String,
    },
}

/// What [`decode_surface`] made of one surfaced value — its two silences are
/// not the same fact, so a caller that must tell them apart (the extension
/// law's loud-unknown-class clause) can.
pub enum Decoded {
    /// A recognised surface class, ready to apply.
    Surface(Surface),
    /// An observation `landing` declines to render: a known shape this host
    /// chooses to keep off the rail, not a decode failure.
    Landed,
    /// No surface class recognises this value's shape at all.
    Unknown,
}

/// Decode one surfaced `Value` into the [`Surface`] it names — the single
/// decoder both delivery regimes share, so the live sink's events and the
/// deferred sink's later `deliver` cannot drift.
///
/// Every class riding the one `exarch-surface` channel is a variant with a
/// distinct label, so the arm order below carries no meaning.  Anything else
/// is [`Decoded::Unknown`].
///
/// The register is not among them: a pin is *state*, keyed to a slot and
/// overwritten in place, and `exarch-pins` is its only door.
pub fn decode_surface(ev: &FOValue) -> Decoded {
    if let Some(event) = Observation::from_surface(ev) {
        // Core reports every observation it makes and judges none of them;
        // `landing` is where this host says which it wants, and a write is
        // wanted as the change it made.  One core dispatch is one
        // observation, so a rejected one is dropped outright rather than
        // offered to the decoders below — deliberately, so it is `Landed`,
        // not `Unknown`.
        match (Change::of(&event.what), landing(&event.what)) {
            (Some(change), _) => Decoded::Surface(Surface::Change(change)),
            (None, Some(_)) => Decoded::Surface(Surface::Observation(Box::new(event))),
            (None, None) => Decoded::Landed,
        }
    } else if let Some(change) = value_to_edit(ev) {
        Decoded::Surface(Surface::Change(change))
    } else if let Some(notice) = ral_core::types::Notice::from_surface(ev) {
        Decoded::Surface(Surface::Notice(notice))
    } else if let Some(card) = value_to_card(ev) {
        Decoded::Surface(Surface::Card(card))
    } else if let Some((cmd, outcome)) = value_to_done(ev) {
        Decoded::Surface(Surface::Done { cmd, outcome })
    } else {
        Decoded::Unknown
    }
}

/// The extension law's loud-unknown-class note, worded once for every
/// [`Decoded::Unknown`] site — the live sink
/// ([`crate::agent::desk::SurfaceApplier::live`]) and a deferred batch's
/// replay alike.
pub(crate) fn unknown_surface_note(shape: impl std::fmt::Display) -> crate::record::Forensic {
    crate::record::Forensic::SystemNote {
        text: format!("surface: no rendering for a {shape} value; it was dropped"),
    }
}

/// The deferred half of `surface`: the session-lived [`DeferredSink`] a
/// detached `spawn` worker flushes its buffered batch to at completion.  Not a
/// second channel — the live sink's own vocabulary, posted through the
/// session's envelope as a [`Stamped::Surface`] to render at the next boundary.
///
/// A worker settling mid-`/clear` cannot decide its own staleness, since
/// composing the batch and pushing it are two steps a `/clear` can fall
/// between; so the sink holds a [`Stamp`] minted at construction and always
/// posts, leaving the pop to reject a stale one at the consuming edge.
struct InboxDeferred {
    stamp: Stamp,
    /// The **root** session's id: a spawn worker registers no tab of its own,
    /// so its cards must land in the root scrollback.
    root: AgentId,
}

impl DeferredSink for InboxDeferred {
    fn deliver(&self, values: Vec<FOValue>) {
        self.stamp.post(Stamped::Surface {
            id: self.root,
            values,
        });
    }
}

/// Build the [`DeferredSink`] a tool run installs, over `emit`'s session inbox.
/// Core clones it into each worker's run state, so a nested `spawn` inherits it
/// and flushes at its own completion.
pub(crate) fn deferred_sink(emit: &Emitter) -> Arc<dyn DeferredSink> {
    Arc::new(InboxDeferred {
        stamp: emit.mailbox().stamp(),
        root: emit.id(),
    })
}

/// Evaluate `cmd` against `transport` under `caps`, capturing stdout and
/// stderr.  Everything crosses the engine protocol: a `Source` `Run` out, a
/// stream of surface events drained to the bus, one terminal `Report` back.
///
/// `source` names the call's text wherever a position is shown — a
/// diagnostic, a trail site, a worker spawned from it.  `host` is the host's
/// side of this run — the test harness's bare `IdentityTransport` hands a
/// pin-less, desk-less [`crate::agent::desk::SurfaceApplier`], every real
/// caller a [`crate::agent::desk::RunHost`].
pub(crate) fn run_shell(
    transport: &dyn ral_core::carrier::Transport,
    caps: &ral_core::capability::GrantStack,
    source: &str,
    cmd: &str,
    timeout_secs: u64,
    host: Arc<dyn ral_core::carrier::Host>,
) -> Result<ral_core::protocol::Report, ral_core::carrier::Severed> {
    // Trace-only timing.
    #[cfg(debug_assertions)]
    let tool_start = std::time::Instant::now();

    use ral_core::protocol::Run;

    let run = Run {
        caps: caps.clone(),
        wall: Some(Duration::from_secs(timeout_secs)),
        deferred_lease: Some(ral_core::types::WorkerLease {
            idle: DETACHED_WORKER_CEILING,
            backstop: DETACHED_WORKER_BACKSTOP,
        }),
        worker_cap: Some(LIVE_WORKER_CAP),
        ..Run::captured(cmd, source)
    };

    let report = ral_core::carrier::dispatch_to_report(transport, run, host);
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
const VALUE_PRINT_PARAMS: ral_core::builtins::PrintParams = ral_core::builtins::PrintParams {
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
        other => Some(ral_core::builtins::pretty_print(
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
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;
