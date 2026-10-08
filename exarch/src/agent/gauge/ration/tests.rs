use super::*;
use crate::provider::allowance::Consumption;

fn window(hours: u64, fraction: f64) -> Allowance {
    Allowance {
        window: Some(Duration::from_hours(hours)),
        used: Consumption::Fraction(fraction),
        resets_at: None,
    }
}

fn read(ration: &mut Ration, a: &[Allowance], resumes: bool) -> Vec<Warning> {
    ration.climb(&Account::built_in("openrouter"), a, resumes)
}

fn ration(warning: &Warning) -> (u32, bool, bool) {
    let Warning::Model(Reminder::Ration {
        pct,
        windowed,
        resumes,
        ..
    }) = warning
    else {
        panic!("the model is reminded");
    };
    (*pct, *windowed, *resumes)
}

#[test]
fn crossing_ninety_reminds_the_model_once_with_the_outcome_by_resumption() {
    let mut ration_latches = Ration::default();
    assert!(read(&mut ration_latches, &[window(5, 0.80)], true).is_empty());
    let warnings = read(&mut ration_latches, &[window(5, 0.91)], true);
    let [warning] = warnings.as_slice() else {
        panic!("one reminder at 90");
    };
    assert_eq!(ration(warning), (91, true, true));
    assert!(read(&mut ration_latches, &[window(5, 0.92)], true).is_empty());

    let warnings = read(&mut Ration::default(), &[window(5, 0.91)], false);
    assert_eq!(ration(&warnings[0]), (91, true, false));
}

#[test]
fn windows_latch_apart() {
    let mut ration_latches = Ration::default();
    assert_eq!(
        read(
            &mut ration_latches,
            &[window(5, 0.91), window(168, 0.91)],
            true
        )
        .len(),
        2
    );
    assert!(
        read(
            &mut ration_latches,
            &[window(5, 0.91), window(168, 0.91)],
            true
        )
        .is_empty()
    );
    assert_eq!(
        read(
            &mut ration_latches,
            &[window(5, 0.91), window(168, 0.96)],
            true
        )
        .len(),
        1,
        "only the weekly window climbed"
    );
}

#[test]
fn a_purse_is_told_apart_from_a_window() {
    let purse = Allowance {
        window: None,
        used: Consumption::Fraction(0.96),
        resets_at: None,
    };
    let warnings = read(&mut Ration::default(), &[purse], true);
    assert_eq!(ration(&warnings[0]), (96, false, true));
}

#[test]
fn an_allowance_with_no_fraction_is_skipped() {
    let uncapped = Allowance {
        window: None,
        used: Consumption::Counted {
            used: 5,
            limit: None,
            unit: crate::provider::allowance::Unit::Requests,
        },
        resets_at: None,
    };
    assert!(
        read(
            &mut Ration::default(),
            std::slice::from_ref(&uncapped),
            true
        )
        .is_empty()
    );
    assert!(told(&Account::built_in("openrouter"), &[uncapped]).is_empty());
}

#[test]
fn told_formats_the_user_line() {
    let warnings = told(&Account::built_in("openrouter"), &[window(5, 0.80)]);
    let [Warning::User(line)] = warnings.as_slice() else {
        panic!("one line for the user");
    };
    assert!(
        line.starts_with("usage: openrouter at 80%; 5 hours"),
        "{line}"
    );
}
