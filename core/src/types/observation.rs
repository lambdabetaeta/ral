//! One fact observed at a door, and the one record shape it reifies as.
//!
//! The surface rail, the audit trail, `--audit`, and the wire all speak this
//! vocabulary: [`Observation::to_wire`] is the single projection,
//! [`Observation::to_value`] its runtime form, and
//! [`Observation::from_wire`] the inverse, so a host decodes exactly what
//! core built.  The envelope is a record of `site`, `start`, `end` and
//! `principal`; the fact itself is `what`, a variant whose tag is
//! the kind, so no separate `kind` field can disagree with the payload beside
//! it.  On the surface channel, which carries other classes too, the record
//! rides as `` `observed <record> `` ([`Observation::to_surface`]).

use super::audit::{AuditIo, epoch_us};
use super::shell::workers::{LeaseClass, WorkerId};
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::serial::FOValue;
use crate::serial::datum::untag;
use crate::syntax::ast::RedirectMode;
use std::collections::BTreeMap;

/// One fact observed at a door: a command settled, a write committed, a
/// redirect read opened, a capability check decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// `None` where the observing dispatch has no position in a session source.
    pub site: Option<CallSite>,
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

fn string(value: impl Into<String>) -> FOValue {
    FOValue::String {
        value: value.into(),
    }
}

fn int(value: i64) -> FOValue {
    FOValue::Int { value }
}

fn bytes(value: Vec<u8>) -> FOValue {
    FOValue::Bytes { value }
}

/// Keys sorted, as a runtime map would iterate them, so the wire form is the
/// one `Value`'s own encoding would give.
fn record(fields: Vec<(&str, FOValue)>) -> FOValue {
    let mut entries: Vec<(String, FOValue)> =
        fields.into_iter().map(|(k, v)| (k.into(), v)).collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    FOValue::Map { entries }
}

fn tagged(label: &str, payload: FOValue) -> FOValue {
    FOValue::Variant {
        label: label.into(),
        payload: Some(Box::new(payload)),
    }
}

/// `` `just x `` for a field that has a value, `` `none `` for one that does
/// not: an absent before-image is a fact of its own, not a missing key.
fn optional(v: Option<FOValue>) -> FOValue {
    match v {
        Some(v) => tagged("just", v),
        None => FOValue::Variant {
            label: "none".into(),
            payload: None,
        },
    }
}

/// A source position as ral sees it: `` `just [script, line, col] `` or
/// `` `none ``, shared by the trail and `try`'s error record.
#[allow(
    clippy::cast_possible_wrap,
    reason = "line/col are source positions bounded by source size, far below i64::MAX"
)]
pub(crate) fn site_value(site: Option<&CallSite>) -> FOValue {
    optional(site.map(|s| {
        record(vec![
            ("script", string(s.script.clone())),
            ("line", int(s.line as i64)),
            ("col", int(s.col as i64)),
        ])
    }))
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "line/col were projected from usize source positions"
)]
fn site_of(v: &FOValue) -> Option<CallSite> {
    Some(CallSite {
        script: str_at(v, "script")?,
        line: int_at(v, "line")? as usize,
        col: int_at(v, "col")? as usize,
    })
}

impl Observation {
    /// An instantaneous door: the observation is stamped now, and its window
    /// has no width.
    pub fn instant(site: Option<CallSite>, principal: Option<String>, what: Observed) -> Self {
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
        site: Option<CallSite>,
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
    /// site, byte field or subject is `` `none ``, never a missing key.
    pub fn to_wire(&self) -> FOValue {
        record(vec![
            ("site", site_value(self.site.as_ref())),
            ("start", int(self.start)),
            ("end", int(self.end)),
            (
                "principal",
                string(self.principal.clone().unwrap_or_default()),
            ),
            ("what", tagged(self.what.kind(), self.what.to_payload())),
        ])
    }

    /// [`Self::to_wire`] as a runtime value.
    pub fn to_value(&self) -> Value {
        Value::from(self.to_wire())
    }

    /// Inverse of [`Self::to_wire`]; `None` for anything that is not a record
    /// this module built, so a host decoder can try the next shape.
    pub fn from_wire(v: &FOValue) -> Option<Self> {
        let FOValue::Variant { label, payload } = v.field("what")? else {
            return None;
        };
        Some(Self {
            site: optional_at(v, "site", site_of)?,
            start: int_at(v, "start")?,
            end: int_at(v, "end")?,
            principal: Some(str_at(v, "principal")?).filter(|p| !p.is_empty()),
            what: Observed::from_payload(label, payload.as_deref()?)?,
        })
    }

    /// The tag an observation carries on the surface channel.
    pub const SURFACE_TAG: &str = "observed";

    /// [`Self::to_wire`] tagged for the surface channel, so a host dispatches
    /// on the tag alone.
    pub fn to_surface(&self) -> FOValue {
        tagged(Self::SURFACE_TAG, self.to_wire())
    }

    /// Inverse of [`Self::to_surface`].
    pub fn from_surface(v: &FOValue) -> Option<Self> {
        match untag(v)? {
            (Self::SURFACE_TAG, Some(record)) => Self::from_wire(record),
            _ => None,
        }
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
    fn to_payload(&self) -> FOValue {
        match self {
            Self::Command {
                argv,
                status,
                origin,
                io,
                error,
            } => record(vec![
                (
                    "argv",
                    FOValue::List {
                        items: argv.iter().map(|a| string(a.clone())).collect(),
                    },
                ),
                ("status", int(i64::from(*status))),
                ("origin", string(origin.as_str())),
                ("stdout", bytes(io.stdout.clone())),
                ("stderr", bytes(io.stderr.clone())),
                ("error", string(error.clone().unwrap_or_default())),
            ]),
            Self::Write {
                path,
                mode,
                outcome,
                new_bytes,
                old_bytes,
            } => record(vec![
                ("path", string(path.clone())),
                ("mode", string(mode_str(*mode))),
                ("outcome", string(outcome.as_str())),
                ("new_bytes", optional(new_bytes.clone().map(bytes))),
                ("old_bytes", optional(old_bytes.clone().map(bytes))),
            ]),
            Self::Read { path } => record(vec![("path", string(path.clone()))]),
            Self::Grep { scope, pattern } => record(vec![
                ("scope", string(scope.clone())),
                ("pattern", string(pattern.clone())),
            ]),
            Self::Capability {
                resource,
                decision,
                fields,
            } => record(vec![
                ("resource", string(resource.clone())),
                ("decision", string(decision.as_str())),
                (
                    "fields",
                    FOValue::Map {
                        entries: fields
                            .iter()
                            .map(|(k, v)| (k.clone(), string(v.clone())))
                            .collect(),
                    },
                ),
            ]),
            #[allow(
                clippy::cast_possible_wrap,
                reason = "a worker id is minted from a process-global counter, far below i64::MAX"
            )]
            Self::Worker { id, cmd, class } => record(vec![
                ("id", int(id.0 as i64)),
                ("cmd", string(cmd.clone())),
                ("class", string(lease_class_str(*class))),
            ]),
            Self::Act {
                verb,
                subject,
                payload,
                refused,
            } => record(vec![
                ("verb", string(verb.clone())),
                ("subject", optional(subject.clone().map(string))),
                ("payload", string(payload.clone())),
                ("refused", FOValue::Bool { value: *refused }),
            ]),
        }
    }

    fn from_payload(tag: &str, m: &FOValue) -> Option<Self> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "an exit status was projected from i32 and round-trips exactly"
        )]
        Some(match tag {
            "command" => Self::Command {
                argv: strings_at(m, "argv")?,
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

fn string_of(v: &FOValue) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn bytes_of(v: &FOValue) -> Option<Vec<u8>> {
    v.as_bytes().map(<[u8]>::to_vec)
}

fn str_at(m: &FOValue, key: &str) -> Option<String> {
    string_of(m.field(key)?)
}

fn int_at(m: &FOValue, key: &str) -> Option<i64> {
    m.field(key)?.as_int()
}

fn bool_at(m: &FOValue, key: &str) -> Option<bool> {
    m.field(key)?.as_bool()
}

fn bytes_at(m: &FOValue, key: &str) -> Option<Vec<u8>> {
    bytes_of(m.field(key)?)
}

/// Inverse of [`optional`]: the outer `None` means the field was not the
/// option [`Observation::to_wire`] projects, the inner one the honest absence.
#[allow(
    clippy::option_option,
    reason = "the two layers are different facts: a malformed field and an absent one"
)]
fn optional_at<T>(m: &FOValue, key: &str, of: fn(&FOValue) -> Option<T>) -> Option<Option<T>> {
    match m.field(key)? {
        FOValue::Variant { label, payload } if label == "just" => of(payload.as_deref()?).map(Some),
        FOValue::Variant {
            label,
            payload: None,
        } if label == "none" => Some(None),
        _ => None,
    }
}

fn string_map_at(m: &FOValue, key: &str) -> Option<BTreeMap<String, String>> {
    let FOValue::Map { entries } = m.field(key)? else {
        return None;
    };
    entries
        .iter()
        .map(|(k, v)| Some((k.clone(), string_of(v)?)))
        .collect()
}

fn strings_at(m: &FOValue, key: &str) -> Option<Vec<String>> {
    m.field(key)?.as_list()?.iter().map(string_of).collect()
}

#[cfg(test)]
mod tests {
    use super::super::map::Map;
    use super::*;

    fn site() -> CallSite {
        CallSite {
            script: "run.ral".into(),
            line: 12,
            col: 3,
        }
    }

    fn round_trips(what: Observed) {
        let obs = Observation::spanning(Some(site()), 100, 250, Some("alex".into()), what);
        let back = Observation::from_wire(&obs.to_wire());
        assert_eq!(back.as_ref(), Some(&obs));
    }

    #[test]
    fn an_absent_site_projects_as_none_and_round_trips() {
        let obs = Observation::instant(
            None,
            None,
            Observed::Grep {
                scope: String::new(),
                pattern: "x".into(),
            },
        );
        let value = obs.to_value();
        let Value::Map(m) = &value else {
            panic!("an observation projects as a record")
        };
        assert_eq!(m.get("site"), Some(&Value::from(optional(None))));
        assert_eq!(Observation::from_wire(&obs.to_wire()), Some(obs));
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
    fn the_surface_form_is_the_record_under_its_tag() {
        let obs = Observation::instant(None, None, Observed::Read { path: "a".into() });
        let surfaced = obs.to_surface();
        assert_eq!(
            untag(&surfaced),
            Some((Observation::SURFACE_TAG, Some(&obs.to_wire())))
        );
        assert_eq!(Observation::from_surface(&surfaced), Some(obs.clone()));
        assert!(Observation::from_surface(&obs.to_wire()).is_none());
    }

    #[test]
    fn from_wire_declines_what_it_did_not_build() {
        let what = |v| FOValue::Map {
            entries: vec![("what".into(), v)],
        };
        assert!(
            Observation::from_wire(&FOValue::String {
                value: "plain".into()
            })
            .is_none()
        );
        assert!(Observation::from_wire(&FOValue::Map { entries: vec![] }).is_none());
        assert!(
            Observation::from_wire(&what(FOValue::Variant {
                label: "teleport".into(),
                payload: Some(Box::new(FOValue::Map { entries: vec![] })),
            }))
            .is_none()
        );
        assert!(
            Observation::from_wire(&what(FOValue::String {
                value: "command".into()
            }))
            .is_none(),
            "an untagged `what` is not a fact"
        );
    }

    /// The tag *is* the kind, so no `kind` field stands beside it to disagree
    /// with the payload it labels.
    #[test]
    fn the_tag_is_the_kind_and_stands_alone() {
        let obs = Observation::instant(
            Some(site()),
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
        let obs = Observation::instant(Some(site()), None, what.clone());
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
    /// it on resume: `to_wire` encodes, `serde_json` crosses the log,
    /// `FOValue`'s own `Deserialize` decodes, and `from_wire` rebuilds.  Bytes, the `what` tag, and both legs of an
    /// optional byte field survive intact.
    #[test]
    fn survives_to_wire_fovalue_json_and_back() {
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
            let obs = Observation::spanning(Some(site()), 10, 20, Some("alex".into()), what);
            let json = serde_json::to_vec(&obs.to_wire()).expect("serialise FOValue");
            let back_fo: FOValue = serde_json::from_slice(&json).expect("deserialise FOValue");
            let back = Observation::from_wire(&back_fo).expect("the wire form decodes");
            assert_eq!(back, obs);
        }
    }

    /// A denied check's decision is a field of its own, never an exit status
    /// standing in for one, and its detail is a nested map rather than fields
    /// spliced beside the envelope.
    #[test]
    fn a_capability_decision_projects_as_itself() {
        let obs = Observation::instant(
            Some(site()),
            Some("alex".into()),
            Observed::Capability {
                resource: "exec".into(),
                decision: Decision::Denied,
                fields: BTreeMap::from([("name".to_string(), "curl".to_string())]),
            },
        );
        let (tag, fact) = fact_of(&obs.to_value());
        assert_eq!(tag, "check");
        assert_eq!(fact.get("decision"), Some(&Value::String("denied".into())));
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
