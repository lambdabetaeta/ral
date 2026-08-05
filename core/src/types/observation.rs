//! One fact observed at a door, and the one map shape it reifies as.
//!
//! The surface rail, the audit trail, `--audit`, and the wire all speak this
//! vocabulary: [`Observation::to_value`] is the single projection, and
//! [`Observation::from_value`] its inverse, so a host decodes exactly what
//! core built.

use super::audit::{AuditIo, epoch_us};
use super::map::Map;
use super::shell::workers::{LeaseClass, WorkerId};
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::syntax::ast::RedirectMode;

/// One fact observed at a door: a command settled, a write committed, a
/// redirect read opened, a capability check decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub site: CallSite,
    /// Microseconds since the Unix epoch; equal to `end` at an instantaneous
    /// door.
    pub start: i64,
    pub end: i64,
    /// `$USER` at the time of observing.
    pub principal: String,
    pub what: Observed,
}

/// What was observed.  A command carries one fact whether it was a builtin,
/// an external, or a detached spawn; the door it passed through is `origin`.
#[derive(Debug, Clone, PartialEq)]
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
        /// `Unit` for an external, which has no ral value to hand back.
        value: Value,
    },
    Write {
        path: String,
        mode: RedirectMode,
        outcome: WriteOutcome,
        /// A bounded head of what landed, on commit only.
        new_bytes: Option<Vec<u8>>,
        /// The target's whole prior content, atomic overwrites only.
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
        /// Per-resource detail, spliced into the projected map beside
        /// `resource` and `decision`.
        fields: Map,
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

/// How a capability check settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
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
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Flagged => "flagged",
        }
    }

    /// An enforced check settles as `Denied` exactly when the policy refused
    /// it; `Flagged` belongs to the advisory checks, which never take this
    /// door.
    pub fn of_allowed(allowed: bool) -> Self {
        if allowed { Self::Allowed } else { Self::Denied }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "allowed" => Self::Allowed,
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

/// Write modes only: a stdin door never settles as a write.
fn mode_str(mode: RedirectMode) -> &'static str {
    match mode {
        RedirectMode::Write => "write",
        RedirectMode::Append => "append",
        RedirectMode::StreamWrite => "stream",
        RedirectMode::Read | RedirectMode::HereString => {
            unreachable!("stdin doors never produce write observations")
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

/// The projected keys every observation carries, plus the ones a capability
/// check owns — what [`Observed::Capability`]'s spliced `fields` must not
/// shadow, and what `from_value` subtracts to recover them.
const ENVELOPE_KEYS: [&str; 7] = ["kind", "script", "line", "col", "start", "end", "principal"];
const CAPABILITY_KEYS: [&str; 2] = ["resource", "decision"];

impl Observation {
    /// An instantaneous door: the observation is stamped now, and its window
    /// has no width.
    pub fn instant(site: CallSite, principal: String, what: Observed) -> Self {
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
    pub fn spanning(
        site: CallSite,
        start: i64,
        end: i64,
        principal: String,
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

    /// The one map shape, shared by the sink broadcast, `audit { }`'s
    /// children, and `--audit`'s JSON.  `error` renders as a string, empty
    /// when the command did not fail — a record field is always present, and
    /// a runtime error message is never empty.  A write's byte fields are
    /// omitted keys rather than nulls when absent.
    pub fn to_value(&self) -> Value {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "line/col are source positions bounded by source size, far below i64::MAX"
        )]
        let mut pairs = vec![
            ("kind".into(), Value::String(self.what.kind().into())),
            ("script".into(), Value::String(self.site.script.clone())),
            ("line".into(), Value::Int(self.site.line as i64)),
            ("col".into(), Value::Int(self.site.col as i64)),
            ("start".into(), Value::Int(self.start)),
            ("end".into(), Value::Int(self.end)),
            ("principal".into(), Value::String(self.principal.clone())),
        ];
        match &self.what {
            Observed::Command {
                argv,
                status,
                origin,
                io,
                error,
                value,
            } => {
                let argv_list = argv.iter().map(|a| Value::String(a.clone())).collect();
                pairs.extend([
                    ("argv".into(), Value::list(argv_list)),
                    ("status".into(), Value::Int(i64::from(*status))),
                    ("origin".into(), Value::String(origin.as_str().into())),
                    ("stdout".into(), Value::Bytes(io.stdout.clone())),
                    ("stderr".into(), Value::Bytes(io.stderr.clone())),
                    (
                        "error".into(),
                        Value::String(error.clone().unwrap_or_default()),
                    ),
                    ("value".into(), value.clone()),
                ]);
            }
            Observed::Write {
                path,
                mode,
                outcome,
                new_bytes,
                old_bytes,
            } => {
                pairs.extend([
                    ("path".into(), Value::String(path.clone())),
                    ("mode".into(), Value::String(mode_str(*mode).into())),
                    ("outcome".into(), Value::String(outcome.as_str().into())),
                ]);
                if let Some(b) = new_bytes {
                    pairs.push(("new_bytes".into(), Value::Bytes(b.clone())));
                }
                if let Some(b) = old_bytes {
                    pairs.push(("old_bytes".into(), Value::Bytes(b.clone())));
                }
            }
            Observed::Read { path } => {
                pairs.push(("path".into(), Value::String(path.clone())));
            }
            Observed::Grep { scope, pattern } => {
                pairs.extend([
                    ("scope".into(), Value::String(scope.clone())),
                    ("pattern".into(), Value::String(pattern.clone())),
                ]);
            }
            Observed::Capability {
                resource,
                decision,
                fields,
            } => {
                pairs.extend([
                    ("resource".into(), Value::String(resource.clone())),
                    ("decision".into(), Value::String(decision.as_str().into())),
                ]);
                pairs.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            Observed::Worker { id, cmd, class } => {
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "a worker id is minted from a process-global counter, far below i64::MAX"
                )]
                pairs.extend([
                    ("id".into(), Value::Int(id.0 as i64)),
                    ("cmd".into(), Value::String(cmd.clone())),
                    (
                        "class".into(),
                        Value::String(lease_class_str(*class).into()),
                    ),
                ]);
            }
            Observed::Act {
                verb,
                subject,
                payload,
                refused,
            } => {
                pairs.push(("verb".into(), Value::String(verb.clone())));
                if let Some(subject) = subject {
                    pairs.push(("subject".into(), Value::String(subject.clone())));
                }
                pairs.extend([
                    ("payload".into(), Value::String(payload.clone())),
                    ("refused".into(), Value::Bool(*refused)),
                ]);
            }
        }
        Value::map(pairs)
    }

    /// The seam-facing projection: total where [`Self::to_value`] is not.
    /// Every handle or closure reachable through a `value` field crosses as a
    /// placeholder instead of vanishing; nothing about the envelope or any
    /// first-order field changes, so a host decoder built against
    /// [`Self::from_value`] reads it unmodified.
    pub fn to_wire(&self) -> Value {
        crate::serial::scrub(&self.to_value(), &crate::serial::no_wire_form)
    }

    /// Inverse of [`Self::to_value`]; `None` for anything that is not a map
    /// this module built, so a host decoder can try the next shape.
    pub fn from_value(v: &Value) -> Option<Self> {
        let Value::Map(m) = v else { return None };
        let what = Observed::from_map(m)?;
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
            principal: str_at(m, "principal")?,
            what,
        })
    }
}

impl Observed {
    /// The `kind` tag naming this variant in the projected map.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::Write { .. } => "write",
            Self::Read { .. } => "read",
            Self::Grep { .. } => "grep",
            Self::Capability { .. } => "capability-check",
            Self::Worker { .. } => "worker",
            Self::Act { .. } => "act",
        }
    }

    fn from_map(m: &Map) -> Option<Self> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "an exit status was projected from i32 and round-trips exactly"
        )]
        Some(match str_at(m, "kind")?.as_str() {
            "command" => Self::Command {
                argv: strings_at(m, "argv"),
                status: int_at(m, "status")? as i32,
                origin: CommandOrigin::parse(&str_at(m, "origin")?)?,
                io: AuditIo {
                    stdout: bytes_at(m, "stdout").unwrap_or_default(),
                    stderr: bytes_at(m, "stderr").unwrap_or_default(),
                },
                error: Some(str_at(m, "error")?).filter(|e| !e.is_empty()),
                value: m.get("value")?.clone(),
            },
            "write" => Self::Write {
                path: str_at(m, "path")?,
                mode: mode_parse(&str_at(m, "mode")?)?,
                outcome: WriteOutcome::parse(&str_at(m, "outcome")?)?,
                new_bytes: bytes_at(m, "new_bytes"),
                old_bytes: bytes_at(m, "old_bytes"),
            },
            "read" => Self::Read {
                path: str_at(m, "path")?,
            },
            "grep" => Self::Grep {
                scope: str_at(m, "scope")?,
                pattern: str_at(m, "pattern")?,
            },
            "capability-check" => Self::Capability {
                resource: str_at(m, "resource")?,
                decision: Decision::parse(&str_at(m, "decision")?)?,
                fields: m
                    .iter()
                    .filter(|(k, _)| {
                        !ENVELOPE_KEYS.contains(&k.as_str())
                            && !CAPABILITY_KEYS.contains(&k.as_str())
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
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
                subject: str_at(m, "subject"),
                payload: str_at(m, "payload")?,
                refused: bool_at(m, "refused")?,
            },
            _ => return None,
        })
    }
}

fn str_at(m: &Map, key: &str) -> Option<String> {
    match m.get(key)? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
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
    match m.get(key)? {
        Value::Bytes(b) => Some(b.clone()),
        _ => None,
    }
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
    use crate::serial::OPAQUE_TAG;

    fn site() -> CallSite {
        CallSite {
            script: "run.ral".into(),
            line: 12,
            col: 3,
        }
    }

    fn round_trips(what: Observed) {
        let obs = Observation::spanning(site(), 100, 250, "alex".into(), what);
        let back = Observation::from_value(&obs.to_value());
        assert_eq!(back.as_ref(), Some(&obs));
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
            value: Value::Unit,
        });
        round_trips(Observed::Command {
            argv: vec!["len".into()],
            status: 0,
            origin: CommandOrigin::Builtin,
            io: AuditIo::default(),
            error: None,
            value: Value::Int(3),
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
                ("op".to_string(), Value::String("write".into())),
                ("path".to_string(), Value::String("/etc/passwd".into())),
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
                "kind".into(),
                Value::String("teleport".into())
            )]))
            .is_none()
        );
    }

    /// A `Handle` with no worker behind it — just enough to exercise
    /// [`Observation::to_wire`]'s totality, never run.
    fn dummy_handle() -> Value {
        use crate::types::{HandleInner, HandleState};
        use std::sync::{Arc, Mutex};
        Value::Handle(HandleInner {
            result: Arc::new(Mutex::new(None)),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(HandleState::Running)),
            stdout_buf: Arc::new(Mutex::new(Vec::new())),
            stderr_buf: Arc::new(Mutex::new(Vec::new())),
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
            cmd: "<test>".into(),
            cancel: crate::process::CancelScope::default(),
        })
    }

    /// A `Handle` reachable through `value` projects to its `opaque`
    /// placeholder rather than vanishing, and the placeholder round-trips
    /// through `from_value` as the tagged `Variant` it is.
    #[test]
    fn to_wire_scrubs_a_handle_and_the_placeholder_round_trips() {
        let obs = Observation::instant(
            site(),
            "alex".into(),
            Observed::Command {
                argv: vec!["spawn".into()],
                status: 0,
                origin: CommandOrigin::Builtin,
                io: AuditIo::default(),
                error: None,
                value: dummy_handle(),
            },
        );
        let wire = obs.to_wire();
        let Value::Map(m) = &wire else {
            panic!("to_wire projects as a map")
        };
        assert_eq!(
            m.get("value"),
            Some(&Value::Variant {
                label: OPAQUE_TAG.to_string(),
                payload: Some(Box::new(Value::map(vec![(
                    "type".to_string(),
                    Value::String("handle".to_string())
                )])))
            })
        );
        let back = Observation::from_value(&wire).expect("the placeholder decodes");
        let Observed::Command { value, .. } = back.what else {
            panic!("expected a command")
        };
        assert_eq!(value, m.get("value").unwrap().clone());
    }

    /// A genuine string equal to the placeholder's own tag is never mistaken
    /// for one: the placeholder is a tagged `Variant`, a string is a plain
    /// leaf, and `to_wire` leaves the string untouched.
    #[test]
    fn a_genuine_string_cannot_impersonate_the_placeholder() {
        let obs = Observation::instant(
            site(),
            "alex".into(),
            Observed::Command {
                argv: vec!["echo".into()],
                status: 0,
                origin: CommandOrigin::Builtin,
                io: AuditIo::default(),
                error: None,
                value: Value::String(OPAQUE_TAG.to_string()),
            },
        );
        let Value::Map(m) = obs.to_wire() else {
            panic!("to_wire projects as a map")
        };
        assert_eq!(m.get("value"), Some(&Value::String(OPAQUE_TAG.to_string())));
    }

    /// A denied check's decision is a field of its own, never an exit status
    /// standing in for one.
    #[test]
    fn a_capability_decision_projects_as_itself() {
        let obs = Observation::instant(
            site(),
            "alex".into(),
            Observed::Capability {
                resource: "exec".into(),
                decision: Decision::of_allowed(false),
                fields: Map::new(),
            },
        );
        let Value::Map(m) = obs.to_value() else {
            panic!("an observation projects as a map")
        };
        assert_eq!(str_at(&m, "decision").as_deref(), Some("denied"));
        assert!(!m.contains_key("status"));
    }
}
