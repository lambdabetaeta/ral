use super::*;

fn started(model: &str, window: Option<u64>) -> Forensic {
    Forensic::ModelChanged {
        model: model.into(),
        context_window: window,
        label: "p".into(),
        service: "s".into(),
        account: "a".into(),
        via: None,
    }
}

fn resumed(model: &str, window: Option<u64>) -> Forensic {
    Forensic::SessionResumed {
        model: model.into(),
        context_window: window,
        label: "p".into(),
        service: "s".into(),
        account: "a".into(),
        at_unix_ms: 0,
    }
}

#[test]
fn a_narrowed_window_survives_a_resume_on_the_same_model() {
    let mut selection = Selection::default();
    selection.step(&started("m", Some(200_000)));
    selection.step(&Forensic::WindowNarrowed {
        context_window: 100_000,
    });
    selection.step(&resumed("m", Some(200_000)));
    assert_eq!(selection.window(), Some(100_000));
    assert_eq!(selection.learned(), Some(("m".into(), 100_000)));
}

#[test]
fn a_narrowed_window_is_dropped_by_another_model_or_a_switch() {
    let mut selection = Selection::default();
    selection.step(&started("m", Some(200_000)));
    selection.step(&Forensic::WindowNarrowed {
        context_window: 100_000,
    });
    selection.step(&resumed("n", Some(300_000)));
    assert_eq!(selection.window(), Some(300_000));
    assert_eq!(selection.learned(), None);

    selection.step(&Forensic::WindowNarrowed {
        context_window: 50_000,
    });
    selection.step(&started("n", Some(300_000)));
    assert_eq!(selection.window(), Some(300_000));
    assert_eq!(selection.learned(), None);
    assert_eq!(selection.model(), Some(("n", "p")));
}

#[test]
fn job_and_engine_forensics_preserve_the_selection() {
    for forensic in [
        Forensic::JobOpened,
        Forensic::JobSettled {
            job: crate::record::JobId::from(42),
            stopped: true,
        },
        Forensic::EngineSevered {
            cause: ral_core::carrier::Severed::Closed("unexpected EOF".into()),
        },
        Forensic::TeardownFailed {
            failure: "engine end was not certified".into(),
        },
    ] {
        let mut empty = Selection::default();
        empty.step(&forensic);
        assert_eq!(empty.model(), None);
        assert_eq!(empty.window(), None);
        assert_eq!(empty.learned(), None);

        let mut selected = Selection::default();
        selected.step(&started("m", Some(200_000)));
        selected.step(&Forensic::WindowNarrowed {
            context_window: 100_000,
        });
        selected.step(&forensic);
        assert_eq!(selected.model(), Some(("m", "p")));
        assert_eq!(selected.window(), Some(100_000));
        assert_eq!(selected.learned(), Some(("m".into(), 100_000)));
    }
}
