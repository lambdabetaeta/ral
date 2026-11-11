//! The laws every carrier shares: the event stream's stash and the
//! severance codes.
use super::*;

mod event_receiver_tests {
    use super::*;

    fn receiver() -> (EventReceiver, mpsc::Sender<(DispatchId, Event)>) {
        let (tx, rx) = mpsc::channel();
        (EventReceiver::new(rx), tx)
    }

    /// The stash precedence `recv` gives, proven on the non-blocking path.
    #[test]
    fn try_recv_drains_the_stash_before_the_channel() {
        let (receiver, tx) = receiver();
        let channelled = (DispatchId(2), Event::Surface(FOValue::Unit));
        let stashed = (DispatchId(1), Event::Surface(FOValue::Unit));
        tx.send(channelled.clone()).unwrap();
        receiver.stash.lock().unwrap().push_back(stashed.clone());

        assert_eq!(receiver.try_recv(), Some(stashed));
        assert_eq!(receiver.try_recv(), Some(channelled));
    }
}

/// The severance codes are load-bearing text: a front-end prints them beside
/// its one-sentence failure so that a person who cannot read a log still has
/// something exact to quote.  These assertions exist to make a rename a
/// deliberate act with a failing test attached, rather than a tidy-up nobody
/// notices until a support thread stops matching.
mod severance_codes {
    use super::*;

    #[test]
    fn every_severance_has_its_own_settled_code() {
        let codes = [
            (
                Severed::Refused(WireError::of(&Error::new(crate::fact::Reason::Protocol(
                    Box::new(crate::fact::ProtocolFailure::VersionMismatch(
                        crate::fact::ProtocolFailureVersionMismatch {
                            front: 9,
                            engine: 10,
                        },
                    )),
                ))))
                .code(),
                "engine-refused",
            ),
            (
                Severed::Closed(ConnectionClosed::Eof(crate::fact::ProtocolPeer::Engine)).code(),
                "engine-closed",
            ),
            (
                Severed::Silent(Duration::from_secs(30)).code(),
                "engine-silent",
            ),
            (
                Severed::Faulted(ConnectionFault::ReadingForDispatch).code(),
                "engine-faulted",
            ),
        ];
        for (got, want) in codes {
            assert_eq!(got, want, "a severance code may not be renamed in place");
        }
        let mut distinct: Vec<_> = codes.iter().map(|(got, _)| *got).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), codes.len(), "two severances share a code");
    }
}

mod typed_readings {
    use super::*;
    use crate::fact::{ConnectionFaultProbeAnswer, DecodeFailure, ProbeSubject};

    struct ReadingTransport {
        ends: Ends,
        answer: FOValue,
    }

    impl ReadingTransport {
        fn new(answer: FOValue) -> Self {
            let (_tx, rx) = mpsc::channel();
            Self {
                ends: Ends::new(
                    ControlSender::new(ControlRoute::Wire(Weak::new())),
                    EventReceiver::new(rx),
                    Arc::default(),
                ),
                answer,
            }
        }
    }

    impl Transport for ReadingTransport {
        fn ends(&self) -> &Ends {
            &self.ends
        }
        fn dispatch(&self, id: DispatchId, _: Run, _: &Arc<dyn Host>, _: Option<&Scope>) {
            self.ends
                .events
                .stash
                .lock_ignore_poison()
                .push_back((id, Event::Reading(Ok(self.answer.clone()))));
        }
        fn probe(&self, _: &Probe) -> Result<FOValue, ProbeError> {
            Ok(self.answer.clone())
        }
    }

    #[test]
    fn malformed_probe_answers_keep_the_actual_subject_and_original_decode_failure() {
        let value = FOValue::Int { value: -1 };
        let expected: DecodeFailure = Option::<String>::decode(&value).unwrap_err();
        let transport = ReadingTransport::new(value);
        let cause = Severed::Faulted(ConnectionFault::ProbeAnswer(ConnectionFaultProbeAnswer {
            probe: ProbeSubject::EnvVar("HOME".into()),
            cause: expected,
        }));
        assert_eq!(
            transport.env_var("HOME"),
            Err(ProbeError::Severed(cause.clone()))
        );
        assert_eq!(transport.severed(), Some(cause));
    }

    #[test]
    fn a_reading_for_dispatch_is_a_typed_terminal_fault() {
        let transport = ReadingTransport::new(FOValue::Unit);
        let report = dispatch_to_report(
            &transport,
            Run::captured("1", crate::source::SourceOrigin::named("<test>")),
            Arc::new(()),
            None,
        );
        assert_eq!(
            report,
            Err(Severed::Faulted(ConnectionFault::ReadingForDispatch))
        );
    }
}
