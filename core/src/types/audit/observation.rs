//! The constructors of an [`Observation`]: stamping a fact with a clock.  The
//! records themselves are `fact::observation`'s.

use super::{AuditIo, epoch_us};
use crate::fact::{Command, CommandOrigin, Observation, Observed};
use crate::source::CallSite;

impl Observation {
    /// An instantaneous door: the observation is stamped now, and its window
    /// has no width.
    pub fn instant(site: Option<CallSite>, principal: Option<String>, what: Observed) -> Self {
        let now = epoch_us();
        Self::spanning(site, now, now, principal, what)
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
}

impl Observed {
    /// The one place a [`Command`] fact is built, so every door that mints
    /// one spells its argv the same way: the shown name, then its arguments.
    pub fn command(
        shown: &str,
        args: impl IntoIterator<Item = String>,
        status: i32,
        origin: CommandOrigin,
        io: AuditIo,
        error: Option<String>,
    ) -> Self {
        Self::Command(Command {
            argv: std::iter::once(shown.to_string()).chain(args).collect(),
            status,
            origin,
            stdout: io.stdout.into(),
            stderr: io.stderr.into(),
            error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{Act, Check, Decision, Grep, Read, Resource, Worker, Write, WriteOutcome};
    use crate::first_order::datum::{Datum, tag, untag};
    use crate::first_order::{Bytes, FOValue};
    use crate::ir::WriteMode;
    use crate::types::{LeaseClass, Map, Value, WorkerId};
    use std::collections::BTreeMap;

    fn site() -> CallSite {
        CallSite {
            script: "run.ral".into(),
            line: 12,
            col: 3,
        }
    }

    fn round_trips(what: Observed) {
        let obs = Observation::spanning(Some(site()), 100, 250, Some("alex".into()), what);
        assert_eq!(Observation::decode(&obs.clone().encode()), Ok(obs));
    }

    fn command(origin: CommandOrigin, io: AuditIo, error: Option<&str>) -> Observed {
        let argv = ["git", "status"].map(String::from);
        let [shown, arg] = argv;
        Observed::command(&shown, [arg], 128, origin, io, error.map(Into::into))
    }

    fn every_kind() -> Vec<Observed> {
        vec![
            command(
                CommandOrigin::External,
                AuditIo {
                    stdout: b"out".to_vec(),
                    stderr: b"err".to_vec(),
                },
                Some("spawn failed"),
            ),
            command(CommandOrigin::Detached, AuditIo::default(), None),
            Observed::Write(Write {
                path: "out.txt".into(),
                mode: WriteMode::Append,
                outcome: WriteOutcome::Committed,
                new_bytes: Some(b"new".as_slice().into()),
                old_bytes: Some(b"old".as_slice().into()),
            }),
            Observed::Write(Write {
                path: "out.txt".into(),
                mode: WriteMode::Stream,
                outcome: WriteOutcome::Aborted,
                new_bytes: None,
                old_bytes: None,
            }),
            Observed::Read(Read {
                path: "in.txt".into(),
            }),
            Observed::Grep(Grep {
                scope: "src/".into(),
                pattern: "TODO".into(),
            }),
            Observed::Check(Check::new(
                Resource::Fs,
                BTreeMap::from([
                    ("op".to_string(), "write".to_string()),
                    ("path".to_string(), "/etc/passwd".to_string()),
                ]),
            )),
            Observed::Check(Check::new(
                Resource::Deputy,
                BTreeMap::from([("prefix".to_string(), "/tmp".to_string())]),
            )),
            Observed::Worker(Worker {
                id: WorkerId(7),
                cmd: "watch build".into(),
                class: LeaseClass::Worker,
            }),
            Observed::Worker(Worker {
                id: WorkerId(8),
                cmd: "service tail".into(),
                class: LeaseClass::Durable,
            }),
            Observed::Act(Act {
                verb: "spawn".into(),
                subject: Some("reviewer".into()),
                payload: "check the diff".into(),
                refused: false,
            }),
            Observed::Act(Act {
                verb: "reply".into(),
                subject: None,
                payload: "done".into(),
                refused: true,
            }),
        ]
    }

    #[test]
    fn every_variant_round_trips_through_its_encoding() {
        every_kind().into_iter().for_each(round_trips);
    }

    #[test]
    fn an_absent_site_encodes_as_none_and_round_trips() {
        let obs = Observation::instant(
            None,
            None,
            Observed::Grep(Grep {
                scope: String::new(),
                pattern: "x".into(),
            }),
        );
        let Value::Map(m) = Value::from(obs.clone().encode()) else {
            panic!("an observation encodes as a record")
        };
        assert_eq!(
            m.get("site").as_deref(),
            Some(&Value::variant("none", None))
        );
        assert_eq!(
            m.get("principal").as_deref(),
            Some(&Value::variant("none", None))
        );
        assert_eq!(Observation::decode(&obs.clone().encode()), Ok(obs));
    }

    /// The tag and the record behind it, out of an encoding's `what`.
    fn fact_of(v: &Value) -> (String, Map) {
        let Value::Map(m) = v else {
            panic!("an observation encodes as a record")
        };
        let what = m.get("what");
        let Some(Value::Variant {
            label,
            payload: Some(fact),
        }) = what.as_deref()
        else {
            panic!("`what` is a tagged fact")
        };
        let Value::Map(fact) = fact.as_ref() else {
            panic!("a fact encodes as a record")
        };
        (label.to_string(), fact.clone())
    }

    #[test]
    fn the_surface_form_is_the_record_under_its_tag() {
        let obs = Observation::instant(None, None, Observed::Read(Read { path: "a".into() }));
        let surfaced = obs.to_surface();
        assert_eq!(
            untag(&surfaced),
            Some((Observation::SURFACE_TAG, Some(&obs.clone().encode())))
        );
        assert_eq!(Observation::from_surface(&surfaced), Some(obs.clone()));
        assert!(Observation::from_surface(&obs.encode()).is_none());
    }

    #[test]
    fn decode_refuses_what_it_did_not_build() {
        let what = |v| FOValue::Map {
            entries: vec![("what".into(), v)],
        };
        let plain = FOValue::String {
            value: "plain".into(),
        };
        assert!(Observation::decode(&plain).is_err());
        assert!(Observation::decode(&FOValue::Map { entries: vec![] }).is_err());
        assert!(
            Observation::decode(&what(tag(
                "teleport",
                Some(FOValue::Map { entries: vec![] })
            )))
            .is_err()
        );
        assert!(
            Observation::decode(&what(FOValue::String {
                value: "command".into()
            }))
            .is_err(),
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
            Observed::Read(Read {
                path: "in.txt".into(),
            }),
        );
        let value = Value::from_datum(obs);
        let (tag, fact) = fact_of(&value);
        assert_eq!(tag, "read");
        assert_eq!(fact.get("path").as_deref(), Some(&Value::string("in.txt")));
        let Value::Map(m) = &value else {
            unreachable!()
        };
        assert!(!m.contains_key("kind"));
    }

    /// An absent before-image is `` `none ``, not a missing key: a reader must
    /// tell "unknown" from "empty", and both round-trip.
    #[test]
    fn an_absent_byte_field_encodes_as_none() {
        let what = Observed::Write(Write {
            path: "out.txt".into(),
            mode: WriteMode::Write,
            outcome: WriteOutcome::Committed,
            new_bytes: Some(Bytes::default()),
            old_bytes: None,
        });
        let obs = Observation::instant(Some(site()), None, what.clone());
        let (_, fact) = fact_of(&Value::from(obs.encode()));
        assert_eq!(
            fact.get("new-bytes").as_deref(),
            Some(&Value::variant("some", Some(Value::bytes(Vec::new())))),
            "an empty new side is known, and known-empty"
        );
        assert_eq!(
            fact.get("old-bytes").as_deref(),
            Some(&Value::variant("none", None))
        );
        round_trips(what);
    }

    /// The record leg's full round trip, as `Display::Observation` retraces
    /// it on resume: `encode`, `serde_json` across the log, `FOValue`'s own
    /// `Deserialize`, and `decode`.
    #[test]
    fn survives_encode_fovalue_json_and_back() {
        for what in every_kind() {
            let obs = Observation::spanning(Some(site()), 10, 20, Some("alex".into()), what);
            let json = serde_json::to_vec(&obs.clone().encode()).expect("serialise FOValue");
            let back: FOValue = serde_json::from_slice(&json).expect("deserialise FOValue");
            assert_eq!(Observation::decode(&back), Ok(obs));
        }
    }

    /// A check's decision is a field of its own, never an exit status standing
    /// in for one, and its detail is a nested map rather than fields spliced
    /// beside the envelope.
    #[test]
    fn a_check_encodes_its_resources_decision() {
        let obs = Observation::instant(
            Some(site()),
            Some("alex".into()),
            Observed::Check(Check::new(
                Resource::Exec,
                BTreeMap::from([("name".to_string(), "curl".to_string())]),
            )),
        );
        let (tag, fact) = fact_of(&Value::from(obs.encode()));
        assert_eq!(tag, "check");
        assert_eq!(
            fact.get("decision").as_deref(),
            Some(&Value::variant("denied", None))
        );
        assert!(!fact.contains_key("status"));
        assert_eq!(
            fact.get("fields").as_deref(),
            Some(&Value::map(vec![("name".into(), Value::string("curl"))]))
        );
    }

    #[test]
    fn a_denied_deputy_is_not_representable() {
        let mut v = Check::new(Resource::Deputy, BTreeMap::new()).encode();
        let FOValue::Map { entries } = &mut v else {
            unreachable!()
        };
        entries[0].1 = Decision::Denied.encode();
        assert!(Check::decode(&v).is_err());
    }
}
