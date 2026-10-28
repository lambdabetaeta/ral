//! The schedules family.

use super::*;

/// Refused before anything is armed, naming the flag that grants — and
/// naming the tag the model typed, not a wire word it never saw.
#[test]
fn every_schedule_tag_is_refused_in_the_models_own_vocabulary() {
    for (request, verb) in [
        (add_req("1s", "nightly", "wake"), "exarch-schedules `add"),
        (
            Request::Schedules(Schedules::List),
            "exarch-schedules `list",
        ),
        (remove_req("sched-0"), "exarch-schedules `remove"),
    ] {
        let (desk, _agent) = desk();
        let err = desk
            .ask(request)
            .expect_err("every schedule tag is refused without the grant");
        assert!(
            err.message.starts_with(&format!("`{verb}` refused:")),
            "the refusal must name the tag the model typed, got: {}",
            err.message
        );
        assert!(
            err.message.contains("--allow-schedule"),
            "must name the grant flag, got: {}",
            err.message
        );
    }
}

/// Arm one, see the answer already list it with the fields it was given,
/// remove it by label, see the table empty again — every tag answers the
/// table, so no tag needs a second call to learn what it did.
#[test]
fn schedules_lists_what_add_registered() {
    let (desk, _agent) = granted_desk();
    let rows = table(
        desk.ask(add_req("2h", "nightly", "wake"))
            .expect("a valid `add must succeed"),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(str_field(&rows[0], "label"), Some("nightly"));
    assert_eq!(str_field(&rows[0], "trigger"), Some("after 2h"));
    assert!(rows[0].field("id").is_none(), "the table carries no id");

    let listed = table(
        desk.ask(Request::Schedules(Schedules::List))
            .expect("`list must succeed"),
    );
    assert_eq!(listed.len(), 1, "`list sees what `add armed");

    let after_removal = table(
        desk.ask(remove_req("nightly"))
            .expect("`remove must succeed"),
    );
    assert!(
        after_removal.is_empty(),
        "the wakeup must be gone from the very table `remove answers"
    );
}

/// A duplicate label is refused and registers nothing.  Its row goes up after
/// the registry call precisely so `failed` can carry the outcome; emitted
/// before, every schedule would read as one that landed.
#[test]
fn a_duplicate_schedule_is_refused_and_tiers_its_act_row() {
    let (tx, rx) = channel();
    let (mut desk, _agent) = granted_desk();
    desk.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    desk.services.emit = Emitter::new(tx, AgentId::new(0));
    desk.ask(add_req("1s", "nightly", "wake"))
        .expect("the first schedule must succeed");
    let err = desk
        .ask(add_req("1s", "nightly", "wake"))
        .expect_err("a duplicate label must be refused");
    assert!(err.message.contains("nightly"), "got: {}", err.message);
    assert_eq!(desk.services.schedules.list().len(), 1);

    let acts: Vec<(String, bool)> = crate::bus::drain_records(&rx)
        .into_iter()
        .filter_map(|rec| match rec {
            Record::Display(Display::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            }) if verb == "schedule" => {
                assert_eq!(subject.as_deref(), Some("nightly"));
                Some((payload, failed))
            }
            _ => None,
        })
        .collect();
    assert_eq!(acts.len(), 2, "both attempts draw a row: {acts:?}");
    assert!(!acts[0].1, "the schedule that landed is not tiered");
    assert_eq!(acts[0].0, "after 1s", "a landed row carries its trigger");
    assert!(acts[1].1, "the refused schedule is tiered hot");
    assert!(
        acts[1].0.starts_with("refused: "),
        "a refusal states itself in the payload: {:?}",
        acts[1].0
    );
}
