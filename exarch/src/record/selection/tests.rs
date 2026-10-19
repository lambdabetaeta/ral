use super::*;

fn started(model: &str, window: Option<u64>) -> Forensic {
    Forensic::ModelChanged {
        model: model.into(),
        context_window: window,
        label: "p".into(),
        service: "s".into(),
        account: "a".into(),
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
