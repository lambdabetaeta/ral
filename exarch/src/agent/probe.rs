//! Decoding the engine's probe answers into host-side data.
//!
//! A wire seat puts the engine in another process, so live core values never
//! cross it: `ral_core::protocol::answer_probe` encodes, this decodes. A
//! guest is untrusted: a rejected or ill-shaped answer is a protocol fault
//! and severs the seat (`Seat::fault`); nothing here panics on what a guest
//! sends. A severed engine is not a fresh fault, so it returns the cause
//! already recorded instead.

use crate::agent::Avatar;
use ral_core::serial::FOValue;
use ral_core::protocol::{ProbeError, Severed};

/// A `` `workers `` probe row, decoded.  `pub(crate)` so `shell_eval::report`
/// and the nudge quiet-gate can read it across module boundaries.
pub(crate) struct ProbedWorker {
    pub(crate) id: u64,
    pub(crate) cmd: String,
    pub(crate) class: ral_core::types::LeaseClass,
    pub(crate) running: bool,
    pub(crate) up_secs: u64,
    pub(crate) idle_secs: u64,
    pub(crate) settled_epoch: Option<u64>,
}

impl Avatar {
    /// # Errors
    /// The engine's severance.
    pub(super) fn probe_workers(&self) -> Result<Vec<ProbedWorker>, Severed> {
        let items = match self.seat.transport().probe(FOValue::Variant {
            label: "workers".into(),
            payload: None,
        }) {
            Ok(FOValue::List { items }) => items,
            Err(ProbeError::Severed(s)) => return Err(s),
            other => {
                return Err(self
                    .seat
                    .fault(Severed::Faulted(format!("`workers probe answered {other:?}"))));
            }
        };
        items
            .into_iter()
            .map(|item| {
                let FOValue::Map { entries } = item else {
                    return Err(self
                        .seat
                        .fault(Severed::Faulted("`workers probe row must be a Map".into())));
                };
                let field = |key: &str| {
                    entries
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.clone())
                };
                let int_field = |key: &str| -> Result<u64, Severed> {
                    match field(key) {
                        Some(FOValue::Int { value }) => {
                            #[allow(
                                clippy::cast_sign_loss,
                                reason = "probe integers (id, up-secs, idle-secs) are non-negative counters"
                            )]
                            let v = value as u64;
                            Ok(v)
                        }
                        other => Err(self.seat.fault(Severed::Faulted(format!(
                            "`workers row `{key} must be an Int, got {other:?}"
                        )))),
                    }
                };
                let id = int_field("id")?;
                let cmd = match field("cmd") {
                    Some(FOValue::String { value }) => value,
                    other => {
                        return Err(self.seat.fault(Severed::Faulted(format!(
                            "`workers row `cmd must be a String, got {other:?}"
                        ))));
                    }
                };
                let class = match field("class") {
                    Some(FOValue::String { value }) => match value.as_str() {
                        "worker" => ral_core::types::LeaseClass::Worker,
                        "durable" => ral_core::types::LeaseClass::Durable,
                        other => {
                            return Err(self.seat.fault(Severed::Faulted(format!(
                                "`workers row `class must name a lease class, got {other}"
                            ))));
                        }
                    },
                    other => {
                        return Err(self.seat.fault(Severed::Faulted(format!(
                            "`workers row `class must be a String, got {other:?}"
                        ))));
                    }
                };
                let bool_field = |key: &str| -> Result<bool, Severed> {
                    match field(key) {
                        Some(FOValue::Bool { value }) => Ok(value),
                        other => Err(self.seat.fault(Severed::Faulted(format!(
                            "`workers row `{key} must be a Bool, got {other:?}"
                        )))),
                    }
                };
                let running = bool_field("running")?;
                let up_secs = int_field("up-secs")?;
                let idle_secs = int_field("idle-secs")?;
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
                            other => {
                                return Err(self.seat.fault(Severed::Faulted(format!(
                                    "`workers row `settled-epoch's `some must carry an Int, got {other:?}"
                                ))));
                            }
                        }
                    }
                    Some(FOValue::Variant { label, .. }) if label == "none" => None,
                    other => {
                        return Err(self.seat.fault(Severed::Faulted(format!(
                            "`workers row `settled-epoch must be a Variant, got {other:?}"
                        ))));
                    }
                };
                Ok(ProbedWorker {
                    id,
                    cmd,
                    class,
                    running,
                    up_secs,
                    idle_secs,
                    settled_epoch,
                })
            })
            .collect()
    }

    /// The `` `env-var `` probe, decoded.  Answers the engine's environment —
    /// its dynamic overlay over its own process env — which on a wire seat is
    /// not this process's.
    ///
    /// # Errors
    /// The engine's severance.
    pub(super) fn probe_env_var(&self, name: &str) -> Result<Option<String>, Severed> {
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
                FOValue::String { value } => Ok(Some(value)),
                other => Err(self.seat.fault(Severed::Faulted(format!(
                    "`env-var probe's `some must carry a String, got {other:?}"
                )))),
            },
            Ok(FOValue::Variant { label, .. }) if label == "none" => Ok(None),
            Err(ProbeError::Severed(s)) => Err(s),
            other => Err(self.seat.fault(Severed::Faulted(format!(
                "`env-var probe answered unexpectedly: {other:?}"
            )))),
        }
    }
}
