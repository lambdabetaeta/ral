// These drive a real `--engine` child, never an in-process
// `engine_session` thread: that faces its process's signals, so a
// same-process engine would race the ambient cancel cells against whatever
// sibling test in this lib binary is mid-run, and core's lock over those
// cells is unreachable from here.
#[cfg(unix)]
mod wire;

#[test]
fn read_rejection_keeps_its_actual_probe_subject_and_cause() {
    use super::*;
    use ral_core::fact::ProbeFailure;

    for subject in [
        ProbeSubject::Workers,
        ProbeSubject::EnvVar("EXARCH_SCRATCH".into()),
        ProbeSubject::PathBytes("/scratch".into()),
    ] {
        let engine = Engine::adopted(crate::boot::test_transport());
        let failure = engine
            .read::<()>(subject.clone(), |_| {
                Err(ProbeError::Rejected(ProbeFailure::Busy))
            })
            .unwrap_err();
        assert_eq!(
            failure,
            Severed::Faulted(ConnectionFault::ProbeRejected(
                ConnectionFaultProbeRejected {
                    probe: subject,
                    cause: ProbeFailure::Busy
                }
            ))
        );
        assert_eq!(engine.severed(), Some(failure));
    }
}

#[test]
fn read_severance_and_computation_refusal_keep_the_original_envelope() {
    use super::*;
    use ral_core::fact::{ProtocolFailure, ProtocolFailureVersionMismatch};
    use ral_core::protocol::WireError;

    let original = Error::new(Reason::Protocol(Box::new(
        ProtocolFailure::VersionMismatch(ProtocolFailureVersionMismatch {
            front: 43,
            engine: 44,
        }),
    )))
    .with_context("original attach context")
    .with_hint("first")
    .with_hint("second");
    let wire = WireError::of(&original);
    let lost = Severed::Refused(wire);
    let engine = Engine::adopted(crate::boot::test_transport());
    assert_eq!(
        engine.read::<()>(ProbeSubject::Workers, |_| Err(ProbeError::Severed(
            lost.clone()
        ))),
        Err(lost.clone())
    );
    assert!(
        engine.severed().is_none(),
        "a returned severance is not replaced by a new terminal fault"
    );
    let error = failure(EngineStep::Attach, &lost);
    assert_eq!(error.reason, original.reason);
    assert_eq!(error.context(), original.context());
    assert_eq!(error.hints(), original.hints());
    engine.end().unwrap();
}
