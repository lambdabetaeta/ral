//! Reading engine state across the probe rail.
//!
//! The host cannot hold the engine's live types: a `WorkerEntry`'s handle is
//! not transportable across the seat boundary, so state comes back as
//! decoded data rows rather than a reach for the live core value.
//! [`ProbedWorker`] is that decoding, and [`Agent::probe_workers`]/
//! [`Agent::probe_env_var`] are the two probes that produce it.

use crate::agent::Agent;
use ral_core::serial::FOValue;

/// A `` `workers `` probe row, decoded — the fields
/// [`Agent::resource_rows`] and [`Agent::reconcile_service_pins`] actually
/// read off the shell's worker registry, carried as data across the probe
/// rail rather than the live core `WorkerEntry` (whose handle is not
/// transportable). `pub(crate)` so [`crate::bus::card::services_pin_card`] can
/// render it without reaching back for the live core type.
pub(crate) struct ProbedWorker {
    pub(crate) id: u64,
    pub(crate) cmd: String,
    pub(crate) class: ral_core::types::LeaseClass,
    pub(crate) running: bool,
    pub(crate) up_secs: u64,
    pub(crate) idle_secs: u64,
    pub(crate) settled_epoch: Option<u64>,
}

impl Agent {
    /// The `` `workers `` probe, decoded — the fields
    /// [`Self::resource_rows`] and [`Self::reconcile_service_pins`] actually
    /// read, never the live core `WorkerEntry` (a probe answer is data, not
    /// a handle: no live `Mutex`, no cancel scope).
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
                let running = match field("running") {
                    Some(FOValue::Bool { value }) => value,
                    other => unreachable!("`workers row `running must be a Bool, got {other:?}"),
                };
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
                    up_secs,
                    idle_secs,
                    settled_epoch,
                }
            })
            .collect()
    }

    /// The `` `env-var `` probe, decoded: `` `some [value] `` / `` `none ``
    /// back to `Option<String>`. The only two readers of this dynamic env
    /// overlay are [`Self::check_disk_warn`] and [`Self::resource_rows`],
    /// and both are pure: neither ticks a ledger.
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
