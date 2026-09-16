//! One fact observed at a door, and the one record shape it reifies as.
//!
//! The surface rail, the audit trail, `--audit`, and the wire all speak this
//! vocabulary: [`Observation::to_value`] is the single projection, and
//! [`Observation::from_value`] its inverse, so a host decodes exactly what
//! core built.  The envelope is a record of `script`, `line`, `col`, `start`,
//! `end` and `principal`; the fact itself is `what`, a variant whose tag is
//! the kind, so no separate `kind` field can disagree with the payload beside
//! it.

use super::audit::{AuditIo, epoch_us};
use super::map::Map;
use super::shell::workers::{LeaseClass, WorkerId};
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::syntax::ast::RedirectMode;
use std::collections::BTreeMap;

/// One fact observed at a door: a command settled, a write committed, a
/// redirect read opened, a capability check decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub site: CallSite,
    /// Microseconds since the Unix epoch; equal to `end` at an instantaneous
    /// door.
    pub start: i64,
    pub end: i64,
    /// `$USER` at the time of observing; `None` where nothing named one.
    pub principal: Option<String>,
    pub what: Observed,
}

/// What was observed.  A command carries one fact whether it was a builtin,
/// an external, or a detached spawn; the door it passed through is `origin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Command {
        /// Shown name first, then its arguments.
        argv: Vec<String>,
        status: i32,
        origin: CommandOrigin,
        /// Bytes teed off fd 1 and fd 2, empty unless the capture policy is
        /// `Bytes`.
        io: AuditIo,
        /// The runtime's own account of why the command failed — `Some` iff
        /// the outcome was a runtime error.  `io` holds only what the child
        /// wrote; this field is the one place ral speaks in its own voice.
        error: Option<String>,
    },
    Write {
        path: String,
        mode: RedirectMode,
        outcome: WriteOutcome,
        /// The whole content that landed, on an atomic commit small enough to
        /// carry it.  Never a prefix: a card reads a write as the change it
        /// made, and half a side is not a change.
        new_bytes: Option<Vec<u8>>,
        /// The target's whole prior content, empty for a file that did not yet
        /// exist.  `None` means the before-image is *unknown* — a target too
        /// large to read whole — which is not the same fact and must not read
        /// as a creation.
        old_bytes: Option<Vec<u8>>,
    },
    Read {
        path: String,
    },
    Grep {
        scope: String,
        pattern: String,
    },
    Capability {
        /// The resource class checked — `exec`, `fs`, …
        resource: String,
        decision: Decision,
        /// Per-resource detail, a nested map beside `resource` and
        /// `decision`.
        fields: BTreeMap<String, String>,
    },
    /// A worker's birth, filed at `spawn_child` in the same breath as its
    /// registry entry — after the reservation succeeds, so a spawn the cap
    /// refused observes nothing.  The fact a later reader joins against the
    /// registry, or the `` `workers `` probe, to ask what became of it.
    Worker {
        id: WorkerId,
        cmd: String,
        class: LeaseClass,
    },
    /// A harness act, authored host-side at the arm where its outcome is
    /// known — never by the engine.  `subject` is the agent name or schedule
    /// label a spawn or nudge names; `None` for a reply.
    Act {
        verb: String,
        subject: Option<String>,
        payload: String,
        refused: bool,
    },
}

/// Which door a command came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOrigin {
    Builtin,
    External,
    /// A background spawn: the status is 0 by construction, not by
    /// observation, since nothing waits for the child.
    Detached,
}

/// How a recorded capability check settled.  An admitted check is never
/// recorded, so there is no `Allowed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Denied,
    /// Reported, not enforced: `capability::deputy_prefixes` names a confused
    /// deputy without refusing it, so the run continues either way.
    Flagged,
}

/// How a write door settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Committed,
    /// The body broke before commit: an atomic temp is discarded, but a
    /// non-atomic target may be left partly written.
    Aborted,
    /// The open never succeeded, or the atomic rename failed at commit.
    Failed,
}

impl CommandOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::External => "external",
            Self::Detached => "detached",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "builtin" => Self::Builtin,
            "external" => Self::External,
            "detached" => Self::Detached,
            _ => return None,
        })
    }
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Flagged => "flagged",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "denied" => Self::Denied,
            "flagged" => Self::Flagged,
            _ => return None,
        })
    }
}

impl WriteOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "committed" => Self::Committed,
            "aborted" => Self::Aborted,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// Write modes only: `Redirect::new` builds no stdin door on fd 1 or 2, so
/// no read mode ever reaches a write observation.
fn mode_str(mode: RedirectMode) -> &'static str {
    match mode {
        RedirectMode::Write => "write",
        RedirectMode::Append => "append",
        RedirectMode::StreamWrite => "stream",
        RedirectMode::Read | RedirectMode::HereString => {
            unreachable!("`Redirect::new` admits a read mode only on fd 0")
        }
    }
}

fn mode_parse(s: &str) -> Option<RedirectMode> {
    Some(match s {
        "write" => RedirectMode::Write,
        "append" => RedirectMode::Append,
        "stream" => RedirectMode::StreamWrite,
        _ => return None,
    })
}

fn lease_class_str(class: LeaseClass) -> &'static str {
    match class {
        LeaseClass::Worker => "worker",
        LeaseClass::Durable => "durable",
    }
}

fn lease_class_parse(s: &str) -> Option<LeaseClass> {
    Some(match s {
        "worker" => LeaseClass::Worker,
        "durable" => LeaseClass::Durable,
        _ => return None,
    })
}

/// `` `just x `` for a field that has a value, `` `none `` for one that does
/// not: an absent before-image is a fact of its own, not a missing key.
fn optional(v: Option<Value>) -> Value {
    match v {
        Some(v) => Value::Variant {
            label: "just".into(),
            payload: Some(Box::new(v)),
        },
        None => Value::Variant {
            label: "none".into(),
            payload: None,
        },
    }
}

impl Observation {
    /// An instantaneous door: the observation is stamped now, and its window
    /// has no width.
    pub fn instant(site: CallSite, principal: Option<String>, what: Observed) -> Self {
        let now = epoch_us();
        Self {
            site,
            start: now,
            end: now,
            principal,
            what,
        }
    }

    /// A door with a body behind it: the caller stamped `start` before the
    /// body ran and `end` after it settled.
    pub(crate) fn spanning(
        site: CallSite,
        start: i64,
        end: i64,
        principal: Option<String>,
        what: Observed,
    ) -> Self {
        Self {
            site,
            start,
            end,
            principal,
            what,
        }
    }

    /// The one record shape, shared by the sink broadcast, `audit { }`'s
    /// trail, and `--audit`'s JSON.  `error` and `principal` render as
    /// strings, empty when the command did not fail and when nothing named a
    /// principal — a record field is always present, and neither a runtime
    /// error message nor a user name is ever legitimately empty.  An absent
    /// byte field or subject is `` `none ``, never a missing key.
    pub fn to_value(&self) -> Value {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "line/col are source positions bounded by source size, far below i64::MAX"
        )]
        Value::map(vec![
            ("script".into(), Value::String(self.site.script.clone())),
            ("line".into(), Value::Int(self.site.line as i64)),
            ("col".into(), Value::Int(self.site.col as i64)),
            ("start".into(), Value::Int(self.start)),
            ("end".into(), Value::Int(self.end)),
            (
                "principal".into(),
                Value::String(self.principal.clone().unwrap_or_default()),
            ),
            (
                "what".into(),
                Value::Variant {
                    label: self.what.kind().into(),
                    payload: Some(Box::new(self.what.to_payload())),
                },
            ),
        ])
    }

    /// The protocol-facing projection: total where [`Self::to_value`] is not.
    /// Nothing about the envelope or any first-order field changes, so a host
    /// decoder built against [`Self::from_value`] reads it unmodified.
    pub fn to_wire(&self) -> Value {
        crate::serial::scrub(&self.to_value(), &crate::serial::no_wire_form)
    }

    /// Inverse of [`Self::to_value`]; `None` for anything that is not a record
    /// this module built, so a host decoder can try the next shape.
    pub fn from_value(v: &Value) -> Option<Self> {
        let Value::Map(m) = v else { return None };
        let Value::Variant { label, payload } = m.get("what")? else {
            return None;
        };
        let Value::Map(fact) = payload.as_deref()? else {
            return None;
        };
        let what = Observed::from_payload(label, fact)?;
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "line/col were projected from usize source positions"
        )]
        let site = CallSite {
            script: str_at(m, "script")?,
            line: int_at(m, "line")? as usize,
            col: int_at(m, "col")? as usize,
        };
        Some(Self {
            site,
            start: int_at(m, "start")?,
            end: int_at(m, "end")?,
            principal: Some(str_at(m, "principal")?).filter(|p| !p.is_empty()),
            what,
        })
    }
}

impl Observed {
    /// The tag this fact carries in the projected `what`: the kind is the tag,
    /// so there is no second place for it to be recorded.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::Write { .. } => "write",
            Self::Read { .. } => "read",
            Self::Grep { .. } => "grep",
            Self::Capability { .. } => "check",
            Self::Worker { .. } => "worker",
            Self::Act { .. } => "act",
        }
    }

    /// The tagged variant's payload: one closed record per kind.
    fn to_payload(&self) -> Value {
        match self {
            Self::Command {
                argv,
                status,
                origin,
                io,
                error,
            } => {
                let argv_list = argv.iter().map(|a| Value::String(a.clone())).collect();
                Value::map(vec![
                    ("argv".into(), Value::list(argv_list)),
                    ("status".into(), Value::Int(i64::from(*status))),
                    ("origin".into(), Value::String(origin.as_str().into())),
                    ("stdout".into(), Value::Bytes(io.stdout.clone())),
                    ("stderr".into(), Value::Bytes(io.stderr.clone())),
                    (
                        "error".into(),
                        Value::String(error.clone().unwrap_or_default()),
                    ),
                ])
            }
            Self::Write {
                path,
                mode,
                outcome,
                new_bytes,
                old_bytes,
            } => Value::map(vec![
                ("path".into(), Value::String(path.clone())),
                ("mode".into(), Value::String(mode_str(*mode).into())),
                ("outcome".into(), Value::String(outcome.as_str().into())),
                (
                    "new_bytes".into(),
                    optional(new_bytes.clone().map(Value::Bytes)),
                ),
                (
                    "old_bytes".into(),
                    optional(old_bytes.clone().map(Value::Bytes)),
                ),
            ]),
            Self::Read { path } => Value::map(vec![("path".into(), Value::String(path.clone()))]),
            Self::Grep { scope, pattern } => Value::map(vec![
                ("scope".into(), Value::String(scope.clone())),
                ("pattern".into(), Value::String(pattern.clone())),
            ]),
            Self::Capability {
                resource,
                decision,
                fields,
            } => Value::map(vec![
                ("resource".into(), Value::String(resource.clone())),
                ("decision".into(), Value::String(decision.as_str().into())),
                (
                    "fields".into(),
                    Value::map(
                        fields
                            .iter()
                            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                            .collect(),
                    ),
                ),
            ]),
            #[allow(
                clippy::cast_possible_wrap,
                reason = "a worker id is minted from a process-global counter, far below i64::MAX"
            )]
            Self::Worker { id, cmd, class } => Value::map(vec![
                ("id".into(), Value::Int(id.0 as i64)),
                ("cmd".into(), Value::String(cmd.clone())),
                (
                    "class".into(),
                    Value::String(lease_class_str(*class).into()),
                ),
            ]),
            Self::Act {
                verb,
                subject,
                payload,
                refused,
            } => Value::map(vec![
                ("verb".into(), Value::String(verb.clone())),
                (
                    "subject".into(),
                    optional(subject.clone().map(Value::String)),
                ),
                ("payload".into(), Value::String(payload.clone())),
                ("refused".into(), Value::Bool(*refused)),
            ]),
        }
    }

    fn from_payload(tag: &str, m: &Map) -> Option<Self> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "an exit status was projected from i32 and round-trips exactly"
        )]
        Some(match tag {
            "command" => Self::Command {
                argv: strings_at(m, "argv"),
                status: int_at(m, "status")? as i32,
                origin: CommandOrigin::parse(&str_at(m, "origin")?)?,
                io: AuditIo {
                    stdout: bytes_at(m, "stdout").unwrap_or_default(),
                    stderr: bytes_at(m, "stderr").unwrap_or_default(),
                },
                error: Some(str_at(m, "error")?).filter(|e| !e.is_empty()),
            },
            "write" => Self::Write {
                path: str_at(m, "path")?,
                mode: mode_parse(&str_at(m, "mode")?)?,
                outcome: WriteOutcome::parse(&str_at(m, "outcome")?)?,
                new_bytes: optional_at(m, "new_bytes", bytes_of)?,
                old_bytes: optional_at(m, "old_bytes", bytes_of)?,
            },
            "read" => Self::Read {
                path: str_at(m, "path")?,
            },
            "grep" => Self::Grep {
                scope: str_at(m, "scope")?,
                pattern: str_at(m, "pattern")?,
            },
            "check" => Self::Capability {
                resource: str_at(m, "resource")?,
                decision: Decision::parse(&str_at(m, "decision")?)?,
                fields: string_map_at(m, "fields")?,
            },
            #[allow(
                clippy::cast_sign_loss,
                reason = "a worker id was projected from u64 and round-trips exactly"
            )]
            "worker" => Self::Worker {
                id: WorkerId(int_at(m, "id")? as u64),
                cmd: str_at(m, "cmd")?,
                class: lease_class_parse(&str_at(m, "class")?)?,
            },
            "act" => Self::Act {
                verb: str_at(m, "verb")?,
                subject: optional_at(m, "subject", string_of)?,
                payload: str_at(m, "payload")?,
                refused: bool_at(m, "refused")?,
            },
            _ => return None,
        })
    }
}

fn string_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Bytes(b) => Some(b.clone()),
        _ => None,
    }
}

fn str_at(m: &Map, key: &str) -> Option<String> {
    string_of(m.get(key)?)
}

fn int_at(m: &Map, key: &str) -> Option<i64> {
    match m.get(key)? {
        Value::Int(n) => Some(*n),
        _ => None,
    }
}

fn bool_at(m: &Map, key: &str) -> Option<bool> {
    match m.get(key)? {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn bytes_at(m: &Map, key: &str) -> Option<Vec<u8>> {
    bytes_of(m.get(key)?)
}

/// Inverse of [`optional`]: the outer `None` means the field was not the
/// option [`Observation::to_value`] projects, the inner one the honest absence.
#[allow(
    clippy::option_option,
    reason = "the two layers are different facts: a malformed field and an absent one"
)]
fn optional_at<T>(m: &Map, key: &str, of: fn(&Value) -> Option<T>) -> Option<Option<T>> {
    match m.get(key)? {
        Value::Variant { label, payload } if label == "just" => of(payload.as_deref()?).map(Some),
        Value::Variant {
            label,
            payload: None,
        } if label == "none" => Some(None),
        _ => None,
    }
}

fn string_map_at(m: &Map, key: &str) -> Option<BTreeMap<String, String>> {
    let Value::Map(fields) = m.get(key)? else {
        return None;
    };
    fields
        .iter()
        .map(|(k, v)| Some((k.clone(), string_of(v)?)))
        .collect()
}

fn strings_at(m: &Map, key: &str) -> Vec<String> {
    match m.get(key) {
        Some(Value::List(l)) => l
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> CallSite {
        CallSite {
            script: "run.ral".into(),
            line: 12,
            col: 3,
        }
    }

    fn round_trips(what: Observed) {
        let obs = Observation::spanning(site(), 100, 250, Some("alex".into()), what);
        let back = Observation::from_value(&obs.to_value());
        assert_eq!(back.as_ref(), Some(&obs));
    }

    /// The tag and the record behind it, out of a projection's `what`.
    fn fact_of(v: &Value) -> (String, Map) {
        let Value::Map(m) = v else {
            panic!("an observation projects as a record")
        };
        let Some(Value::Variant {
            label,
            payload: Some(fact),
        }) = m.get("what")
        else {
            panic!("`what` is a tagged fact")
        };
        let Value::Map(fact) = fact.as_ref() else {
            panic!("a fact projects as a record")
        };
        (label.clone(), fact.clone())
    }

    #[test]
    fn every_variant_round_trips_through_its_projection() {
        round_trips(Observed::Command {
            argv: vec!["git".into(), "status".into()],
            status: 128,
            origin: CommandOrigin::External,
            io: AuditIo {
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
            },
            error: Some("spawn failed".into()),
        });
        round_trips(Observed::Command {
            argv: vec!["len".into()],
            status: 0,
            origin: CommandOrigin::Builtin,
            io: AuditIo::default(),
            error: None,
        });
        round_trips(Observed::Write {
            path: "out.txt".into(),
            mode: RedirectMode::Append,
            outcome: WriteOutcome::Committed,
            new_bytes: Some(b"new".to_vec()),
            old_bytes: Some(b"old".to_vec()),
        });
        round_trips(Observed::Write {
            path: "out.txt".into(),
            mode: RedirectMode::StreamWrite,
            outcome: WriteOutcome::Aborted,
            new_bytes: None,
            old_bytes: None,
        });
        round_trips(Observed::Read {
            path: "in.txt".into(),
        });
        round_trips(Observed::Grep {
            scope: "src/".into(),
            pattern: "TODO".into(),
        });
        round_trips(Observed::Capability {
            resource: "fs".into(),
            decision: Decision::Denied,
            fields: [
                ("op".to_string(), "write".to_string()),
                ("path".to_string(), "/etc/passwd".to_string()),
            ]
            .into_iter()
            .collect(),
        });
        round_trips(Observed::Worker {
            id: WorkerId(7),
            cmd: "watch build".into(),
            class: LeaseClass::Worker,
        });
        round_trips(Observed::Worker {
            id: WorkerId(8),
            cmd: "service tail".into(),
            class: LeaseClass::Durable,
        });
        round_trips(Observed::Act {
            verb: "spawn".into(),
            subject: Some("reviewer".into()),
            payload: "check the diff".into(),
            refused: false,
        });
        round_trips(Observed::Act {
            verb: "reply".into(),
            subject: None,
            payload: "done".into(),
            refused: true,
        });
    }

    #[test]
    fn from_value_declines_what_it_did_not_build() {
        assert!(Observation::from_value(&Value::String("plain".into())).is_none());
        assert!(Observation::from_value(&Value::map(vec![])).is_none());
        assert!(
            Observation::from_value(&Value::map(vec![(
                "what".into(),
                Value::Variant {
                    label: "teleport".into(),
                    payload: Some(Box::new(Value::map(vec![]))),
                }
            )]))
            .is_none()
        );
        assert!(
            Observation::from_value(&Value::map(vec![(
                "what".into(),
                Value::String("command".into())
            )]))
            .is_none(),
            "an untagged `what` is not a fact"
        );
    }

    /// The tag *is* the kind, so no `kind` field stands beside it to disagree
    /// with the payload it labels.
    #[test]
    fn the_tag_is_the_kind_and_stands_alone() {
        let obs = Observation::instant(
            site(),
            None,
            Observed::Read {
                path: "in.txt".into(),
            },
        );
        let value = obs.to_value();
        let (tag, fact) = fact_of(&value);
        assert_eq!(tag, "read");
        assert_eq!(fact.get("path"), Some(&Value::String("in.txt".into())));
        let Value::Map(m) = &value else {
            panic!("an observation projects as a record")
        };
        assert!(!m.contains_key("kind"));
    }

    /// An absent before-image is `` `none ``, not a missing key: a reader must
    /// tell "unknown" from "empty", and both round-trip.
    #[test]
    fn an_absent_byte_field_projects_as_none() {
        let what = Observed::Write {
            path: "out.txt".into(),
            mode: RedirectMode::Write,
            outcome: WriteOutcome::Committed,
            new_bytes: Some(Vec::new()),
            old_bytes: None,
        };
        let obs = Observation::instant(site(), None, what.clone());
        let (_, fact) = fact_of(&obs.to_value());
        assert_eq!(
            fact.get("new_bytes"),
            Some(&Value::Variant {
                label: "just".into(),
                payload: Some(Box::new(Value::Bytes(Vec::new()))),
            }),
            "an empty new side is known, and known-empty"
        );
        assert_eq!(
            fact.get("old_bytes"),
            Some(&Value::Variant {
                label: "none".into(),
                payload: None,
            })
        );
        round_trips(what);
    }

    /// The record leg's full round trip, as `Display::Observation` retraces
    /// it on resume: `to_wire` scrubs, `FOValue::try_from` encodes,
    /// `serde_json` crosses the log, `FOValue`'s own `Deserialize` decodes,
    /// and `from_value` rebuilds.  Bytes, the `what` tag, and both legs of an
    /// optional byte field survive intact.
    #[test]
    fn survives_to_wire_fovalue_json_and_back() {
        use crate::serial::FOValue;

        for what in [
            Observed::Command {
                argv: vec!["git".into(), "status".into()],
                status: 0,
                origin: CommandOrigin::External,
                io: AuditIo {
                    stdout: b"out".to_vec(),
                    stderr: b"err".to_vec(),
                },
                error: None,
            },
            Observed::Write {
                path: "out.txt".into(),
                mode: RedirectMode::Write,
                outcome: WriteOutcome::Committed,
                new_bytes: Some(b"new".to_vec()),
                old_bytes: None,
            },
        ] {
            let obs = Observation::spanning(site(), 10, 20, Some("alex".into()), what);
            let fo = FOValue::try_from(&obs.to_wire())
                .expect("to_wire scrubs every leaf try_from rejects");
            let json = serde_json::to_vec(&fo).expect("serialise FOValue");
            let back_fo: FOValue = serde_json::from_slice(&json).expect("deserialise FOValue");
            let back =
                Observation::from_value(&Value::from(back_fo)).expect("the wire form decodes");
            assert_eq!(back, obs);
        }
    }

    /// A denied check's decision is a field of its own, never an exit status
    /// standing in for one, and its detail is a nested map rather than fields
    /// spliced beside the envelope.
    #[test]
    fn a_capability_decision_projects_as_itself() {
        let obs = Observation::instant(
            site(),
            Some("alex".into()),
            Observed::Capability {
                resource: "exec".into(),
                decision: Decision::Denied,
                fields: BTreeMap::from([("name".to_string(), "curl".to_string())]),
            },
        );
        let (tag, fact) = fact_of(&obs.to_value());
        assert_eq!(tag, "check");
        assert_eq!(str_at(&fact, "decision").as_deref(), Some("denied"));
        assert!(!fact.contains_key("status"));
        assert_eq!(
            fact.get("fields"),
            Some(&Value::map(vec![(
                "name".into(),
                Value::String("curl".into())
            )]))
        );
    }
}
