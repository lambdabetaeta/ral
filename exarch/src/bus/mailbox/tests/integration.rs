use super::*;
use crate::agent::Trunk;
use crate::agent::testkit::{host, scripted, set_provider, trunk};
use crate::bus::Signal;
use crate::provider::scripted::{Reply, Script};
use crate::record::{Display, Protocol};

#[test]
fn run_commits_job_opened_before_the_first_prompt_record() {
    let root = trunk(Trunk::Converses);
    set_provider(
        &root,
        scripted("test-model", Script::new().then(Reply::text("done"))),
    );
    let handle = root.handle();
    let log = handle.log_dir().path().join(RECORD);
    let host = host(root);
    // Keep the test log's scratch alive after joining.
    let _fleet = host.fleet().clone();
    handle.steer("first prompt".into()).unwrap();
    let deadline = Instant::now() + WAIT;
    let mut settled = false;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(signal) = host.feed().recv_timeout(left) else {
            break;
        };
        if let Signal::Commit(id, commit) = signal
            && id == handle.id()
            && commit.records().iter().any(|record| {
                matches!(
                    record.value(),
                    Record::Forensic(Forensic::JobSettled { .. })
                )
            })
        {
            settled = true;
            break;
        }
    }
    let failures = host.end();
    assert!(settled, "the scripted exchange must settle within {WAIT:?}");
    assert!(failures.is_empty(), "{failures:?}");
    let records = read_records(&log).unwrap();
    let opened = records
        .iter()
        .position(|record| matches!(record, Record::Forensic(Forensic::JobOpened)))
        .unwrap();
    let prompt = records
        .iter()
        .position(|record| matches!(record, Record::Protocol(Protocol::UserPrompt { .. })))
        .unwrap();
    assert!(opened < prompt);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::Forensic(Forensic::JobOpened)))
            .count(),
        1
    );
}

#[test]
fn run_declines_a_stale_worker_batch_at_rest_without_opening_a_job() {
    let root = trunk(Trunk::Converses);
    let handle = root.handle();
    let log = handle.log_dir().path().join(RECORD);
    handle
        .0
        .address
        .post(Input::Surface {
            generation: u64::MAX,
            values: vec![FOValue::Unit],
        })
        .unwrap();
    handle.post_command(Command::Read(Read::Context));
    let host = host(root);
    // Keep the test log's scratch alive after joining.
    let _fleet = host.fleet().clone();
    let deadline = Instant::now() + WAIT;
    let mut declined = false;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(signal) = host.feed().recv_timeout(left) else {
            break;
        };
        if let Signal::Commit(id, commit) = signal
            && id == handle.id()
            && commit
                .records()
                .iter()
                .any(|record| matches!(record.value(), Record::Display(Display::Context { .. })))
        {
            declined = true;
            break;
        }
    }
    let failures = host.end();
    assert!(
        declined,
        "the owner must take up the stale batch within {WAIT:?}"
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!read_records(&log).unwrap().iter().any(|record| matches!(
        record,
        Record::Forensic(Forensic::JobOpened | Forensic::JobSettled { .. })
    )));
}
