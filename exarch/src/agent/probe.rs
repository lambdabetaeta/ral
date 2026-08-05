//! Decoding the engine's probe answers into host-side data.
//!
//! A wire seat puts the engine in another process, so live core values never
//! cross it: `ral_core::transport::answer_probe` encodes, this decodes.
//! Probe only at a run boundary — mid-dispatch answers engine-busy, which
//! panics here.

use crate::agent::Agent;
use ral_core::serial::FOValue;

/// A `` `workers `` probe row, decoded.  `pub(crate)` so
/// [`crate::bus::card::services_pin_card`] can render it.
pub(crate) struct ProbedWorker {
    pub(crate) id: u64,
    pub(crate) cmd: String,
    pub(crate) class: ral_core::types::LeaseClass,
    pub(crate) running: bool,
    /// The engine's own answer, never arithmetic here: core's registry epoch
    /// and [`Agent::ral_epoch`] are different clocks, and neither number
    /// crosses the seam.
    pub(crate) born_this_epoch: bool,
    pub(crate) up_secs: u64,
    pub(crate) idle_secs: u64,
    pub(crate) settled_epoch: Option<u64>,
}

impl Agent {
    pub(super) fn probe_workers(&self) -> Vec<ProbedWorker> {
        let items = match self.seat.transport().probe(FOValue::Variant {
            label: "workers".into(),
            payload: None,
        }) {
            Ok(FOValue::List { items }) => items,
            other => unreachable!("`workers probe must answer a List, got {other:?}"),
        };
        items
            .into_iter()
            .map(|item| {
                let FOValue::Map { entries } = item else {
                    unreachable!("`workers probe row must be a Map");
                };
                let field = |key: &str| {
                    entries
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.clone())
                };
                let int_field = |key: &str| match field(key) {
                    Some(FOValue::Int { value }) => {
                        #[allow(
                            clippy::cast_sign_loss,
                            reason = "probe integers (id, up-secs, idle-secs) are non-negative counters"
                        )]
                        let v = value as u64;
                        v
                    }
                    other => unreachable!("`workers row `{key} must be an Int, got {other:?}"),
                };
                let id = int_field("id");
                let cmd = match field("cmd") {
                    Some(FOValue::String { value }) => value,
                    other => unreachable!("`workers row `cmd must be a String, got {other:?}"),
                };
                let class = match field("class") {
                    Some(FOValue::String { value }) => match value.as_str() {
                        "worker" => ral_core::types::LeaseClass::Worker,
                        "durable" => ral_core::types::LeaseClass::Durable,
                        other => {
                            unreachable!("`workers row `class must name a lease class, got {other}")
                        }
                    },
                    other => unreachable!("`workers row `class must be a String, got {other:?}"),
                };
                let bool_field = |key: &str| match field(key) {
                    Some(FOValue::Bool { value }) => value,
                    other => unreachable!("`workers row `{key} must be a Bool, got {other:?}"),
                };
                let running = bool_field("running");
                let born_this_epoch = bool_field("born-this-epoch");
                let up_secs = int_field("up-secs");
                let idle_secs = int_field("idle-secs");
                let settled_epoch = match field("settled-epoch") {
                    Some(FOValue::Variant { label, payload }) if label == "some" => {
                        match payload.as_deref() {
                            Some(FOValue::Int { value }) => {
                                #[allow(
                                    clippy::cast_sign_loss,
                                    reason = "probe settled-epoch is a non-negative ral-call epoch"
                                )]
                                let epoch = *value as u64;
                                Some(epoch)
                            }
                            other => unreachable!(
                                "`workers row `settled-epoch's `some must carry an Int, got {other:?}"
                            ),
                        }
                    }
                    Some(FOValue::Variant { label, .. }) if label == "none" => None,
                    other => unreachable!(
                        "`workers row `settled-epoch must be a Variant, got {other:?}"
                    ),
                };
                ProbedWorker {
                    id,
                    cmd,
                    class,
                    running,
                    born_this_epoch,
                    up_secs,
                    idle_secs,
                    settled_epoch,
                }
            })
            .collect()
    }

    /// The sentence a raise owes the model about work that outlived it.
    ///
    /// A `defer`red worker is moored to the session root, so the cancel that
    /// unwound this call never reached it: the work runs on while the handle
    /// binding that named it is gone.  `None` when this call deferred nothing
    /// still running — silence is then the whole truth.
    ///
    /// Read through the probe, so the registry may live wherever the seat puts
    /// it, and only ever at a run boundary.
    pub(super) fn surviving_worker_note(&self) -> Option<String> {
        /// Enough to name one call's fan-out without crowding the stderr this
        /// rides on; whatever is left over is counted aloud, never dropped in
        /// silence.
        const NAMED: usize = 5;

        let mut cmds = self
            .probe_workers()
            .into_iter()
            .filter(|w| w.running && w.born_this_epoch)
            .map(|w| format!("`{}`", w.cmd))
            .collect::<Vec<_>>();
        if cmds.is_empty() {
            return None;
        }
        let unnamed = cmds.len().saturating_sub(NAMED);
        cmds.truncate(NAMED);
        let named = cmds.join(", ");
        let overflow = match unnamed {
            0 => String::new(),
            n => format!(", and {n} more not named here"),
        };
        Some(format!(
            "\nwork this call deferred is still running: {named}{overflow}. Its handle binding went \
             with the unwind, so you cannot `await` it.\n"
        ))
    }

    /// The `` `env-var `` probe, decoded.  Answers the engine's environment —
    /// its dynamic overlay over its own process env — which on a wire seat is
    /// not this process's.
    pub(super) fn probe_env_var(&self, name: &str) -> Option<String> {
        match self.seat.transport().probe(FOValue::Variant {
            label: "env-var".into(),
            payload: Some(Box::new(FOValue::String {
                value: name.to_string(),
            })),
        }) {
            Ok(FOValue::Variant {
                label,
                payload: Some(payload),
            }) if label == "some" => match *payload {
                FOValue::String { value } => Some(value),
                other => unreachable!("`env-var probe's `some must carry a String, got {other:?}"),
            },
            Ok(FOValue::Variant { label, .. }) if label == "none" => None,
            other => unreachable!("`env-var probe answered unexpectedly: {other:?}"),
        }
    }
}
