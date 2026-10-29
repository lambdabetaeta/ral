use super::*;
use crate::record::{Display, Forensic, Seq};
use ral_core::own::TestDir;

fn temp_dir(name: &str) -> TestDir {
    OwnDir::for_test(&format!("log-{name}")).expect("a test directory")
}

fn read(path: &Path) -> io::Result<impl Iterator<Item = io::Result<Recorded<Record>>> + use<>> {
    Log::read(File::open(path)?, path)
}

fn error(text: &str) -> Record {
    Record::Forensic(Forensic::Error { text: text.into() })
}

fn several() -> NonEmpty<Record> {
    NonEmpty {
        first: error("alpha"),
        rest: vec![error("bravo"), error("charlie")],
    }
}

fn terminated(mut line: Vec<u8>) -> Vec<u8> {
    line.push(b'\n');
    line
}

fn changed(mut line: Vec<u8>) -> Vec<u8> {
    let at = line.windows(5).position(|text| text == b"bravo").unwrap();
    line[at] = b'B';
    line
}

fn assert_refusal(error: &io::Error, path: &Path, number: u64, reason: &str) {
    let text = error.to_string();
    assert!(text.contains(&ral_core::path::shown(path)), "{text}");
    assert!(text.contains(&format!("line {number}")), "{text}");
    assert!(text.contains(reason), "{text}");
}

#[test]
fn a_commit_round_trips_through_its_line() {
    let dir = temp_dir("round-trip");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let commit = log.append(NonEmpty::one(error("boom"))).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let line = std::str::from_utf8(&bytes).unwrap();
    assert!(line.starts_with("{\"sum\":\""), "{line}");
    assert!(line.contains(",\"commit\":{\"at_unix_ms\":"), "{line}");
    assert!(line.contains("\"records\":["), "{line}");
    assert!(line.ends_with("}\n"), "{line}");
    let (sum, records) = parse_line(&bytes[..bytes.len() - 1]).unwrap();
    assert_eq!(sum, commit.records().first.locus().sum());
    assert_eq!(records.len(), 1);
    assert!(matches!(&records[0], Record::Forensic(Forensic::Error { text }) if text == "boom"));
    let back = read(&path)
        .unwrap()
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].locus(), commit.records().first.locus());
    assert!(
        matches!(back[0].value(), Record::Forensic(Forensic::Error { text }) if text == "boom")
    );
}

#[test]
fn a_commit_of_several_records_round_trips() {
    let dir = temp_dir("several");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let commit = log.append(several()).unwrap();
    assert_eq!(commit.number(), 1);
    let first = commit.records().first.locus();
    for (index, recorded) in commit.records().iter().enumerate() {
        assert_eq!(recorded.locus().seq(), Seq::new(1, index));
        assert_eq!(recorded.locus().bytes(), first.bytes());
        assert_eq!(recorded.locus().sum(), first.sum());
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    let back = read(&path)
        .unwrap()
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(back.len(), 3);
    for ((actual, expected), text) in back
        .iter()
        .zip(commit.records())
        .zip(["alpha", "bravo", "charlie"])
    {
        assert_eq!(actual.locus(), expected.locus());
        assert!(
            matches!(actual.value(), Record::Forensic(Forensic::Error { text: got }) if got == text)
        );
    }
    let next = log.append(NonEmpty::one(error("next"))).unwrap();
    assert_eq!(next.records().first.locus().seq(), Seq::new(2, 0));
}

#[test]
fn a_large_observation_replays_exactly_from_compact_bytes() {
    use ral_core::data::datum::Datum as _;
    use ral_core::fact::{AuditIo, Command, CommandOrigin, Observation, Observed};

    let dir = temp_dir("compact-observation");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let bytes: Vec<u8> = (0..=255).cycle().take(1024 * 1024).collect();
    let observation = Observation::instant(
        None,
        None,
        Observed::Command(Command::new(
            "producer",
            [],
            0,
            CommandOrigin::External,
            AuditIo {
                stdout: bytes.clone(),
                stdout_total: 2 * bytes.len() as u64,
                ..AuditIo::default()
            },
            None,
        )),
    );
    let _commit = log
        .append(NonEmpty::one(Record::Display(Display::Observation {
            value: observation.clone().encode(),
        })))
        .unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < 1_400_000);
    let records = read(&path)
        .unwrap()
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(records.len(), 1);
    let Record::Display(Display::Observation { value }) =
        records.into_iter().next().unwrap().into_value()
    else {
        panic!("the observation replays as an observation")
    };
    assert_eq!(Observation::decode(&value).unwrap(), observation);
}

#[test]
fn a_line_of_an_older_exarch_is_refused_by_name() {
    for interior in [false, true] {
        let dir = temp_dir(&format!("older-{interior}"));
        let path = dir.path().join(RECORD);
        let mut bytes = terminated(
            serde_json::to_vec(&serde_json::json!({
                "at_unix_ms": 0, "record": error("old")
            }))
            .unwrap(),
        );
        if interior {
            bytes.extend(terminated(commit_line(&[error("next")])));
        }
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            Log::complete_len(&mut File::open(&path).unwrap()).unwrap(),
            bytes.len() as u64
        );
        assert_refusal(
            &read(&path).unwrap().next().unwrap().unwrap_err(),
            &path,
            1,
            "older exarch",
        );
    }
}

#[test]
fn a_locus_round_trips_identically_through_append_and_read() {
    let dir = temp_dir("locus-round-trip");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let first = log.append(NonEmpty::one(error("first"))).unwrap();
    let second = log.append(NonEmpty::one(error("second"))).unwrap();
    let back = read(&path)
        .unwrap()
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].locus(), first.records().first.locus());
    assert_eq!(back[1].locus(), second.records().first.locus());
}

#[test]
fn a_changed_commit_fails_its_sum() {
    let line = changed(commit_line(&[error("bravo")]));
    assert!(matches!(unseal(&line), Err(Unsealed::Sum { .. })));
}

#[test]
fn a_torn_last_line_is_never_read() {
    let whole = terminated(commit_line(&several().into_iter().collect::<Vec<_>>()));
    let tails = [
        whole[..whole.len() / 2].to_vec(),
        changed(whole),
        b"garbage\n".to_vec(),
    ];
    for (index, tail) in tails.into_iter().enumerate() {
        let dir = temp_dir(&format!("torn-{index}"));
        let path = dir.path().join(RECORD);
        let head = terminated(commit_line(&[error("head")]));
        let mut bytes = head.clone();
        bytes.extend(tail);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            Log::complete_len(&mut File::open(&path).unwrap()).unwrap(),
            head.len() as u64
        );
        let back = read(&path)
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(back.len(), 1);
        assert!(
            matches!(back[0].value(), Record::Forensic(Forensic::Error { text }) if text == "head")
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    let dir = temp_dir("foreign-tail");
    let path = dir.path().join(RECORD);
    let mut bytes = terminated(commit_line(&[error("head")]));
    bytes.extend(b"{\"not\":\"a commit\"}\n");
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        Log::complete_len(&mut File::open(&path).unwrap()).unwrap(),
        bytes.len() as u64
    );
    let mut back = read(&path).unwrap();
    assert!(back.next().unwrap().is_ok());
    assert_refusal(&back.next().unwrap().unwrap_err(), &path, 2, "older exarch");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn a_corrupt_interior_line_is_refused_by_file_and_line() {
    let faults = [
        (
            changed(terminated(commit_line(&[error("bravo")]))),
            "corrupt",
        ),
        (b"garbage\n".to_vec(), "not JSON"),
        (b"{\"not\":\"a commit\"}\n".to_vec(), "older exarch"),
    ];
    for (index, (fault, reason)) in faults.into_iter().enumerate() {
        let dir = temp_dir(&format!("interior-{index}"));
        let path = dir.path().join(RECORD);
        let mut bytes = terminated(commit_line(&[error("head")]));
        bytes.extend(fault);
        bytes.extend(terminated(commit_line(&[error("last")])));
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            Log::complete_len(&mut File::open(&path).unwrap()).unwrap(),
            bytes.len() as u64
        );
        let mut back = read(&path).unwrap();
        assert!(back.next().unwrap().is_ok());
        let refusal = back.next().unwrap().unwrap_err();
        if reason == "not JSON" {
            assert!(refusal.to_string().contains("corrupt"));
        }
        assert_refusal(&refusal, &path, 2, reason);
    }
}

#[test]
fn an_empty_commit_is_refused() {
    for interior in [false, true] {
        let dir = temp_dir(&format!("empty-{interior}"));
        let path = dir.path().join(RECORD);
        let mut bytes = terminated(commit_line(&[]));
        if interior {
            bytes.extend(terminated(commit_line(&[error("last")])));
        }
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            Log::complete_len(&mut File::open(&path).unwrap()).unwrap(),
            bytes.len() as u64
        );
        assert_refusal(
            &read(&path).unwrap().next().unwrap().unwrap_err(),
            &path,
            1,
            "at least one",
        );
    }
}

#[test]
fn poison_is_latched_unless_a_failure_came_first() {
    for failed in [false, true] {
        let dir = temp_dir(&format!("poison-{failed}"));
        let path = dir.path().join(RECORD);
        let log = Log::create(&dir).unwrap();
        if failed {
            log.fail_after(0);
            let failure = log
                .append(NonEmpty::one(Forensic::Cancelled.into()))
                .unwrap_err();
            assert_eq!(failure.kind(), io::ErrorKind::StorageFull);
            assert!(!log.inner.is_poisoned());
        }
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = log.inner.lock_strict().unwrap();
            panic!("a torn append critical section")
        }));
        assert!(panic.is_err());
        let failure = log.check_available().unwrap_err();
        let text = failure.to_string();
        if failed {
            assert_eq!(failure.kind(), io::ErrorKind::StorageFull);
            assert!(text.contains("injected record write failure"), "{text}");
        } else {
            assert!(text.contains("panicked"), "{text}");
        }
        assert_eq!(
            log.append(NonEmpty::one(Forensic::Cancelled.into()))
                .unwrap_err()
                .to_string(),
            text
        );
        assert_eq!(std::fs::read(path).unwrap(), [] as [u8; 0]);
    }
}

#[test]
fn reopening_for_append_continues_the_commit_numbering() {
    let dir = temp_dir("reopen");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let _first = log.append(NonEmpty::one(error("head"))).unwrap();
    let _second = log.append(several()).unwrap();
    drop(log);
    let old_len = std::fs::metadata(&path).unwrap().len();
    let log = Log::append_to(&dir).unwrap();
    let next = log.append(NonEmpty::one(error("next"))).unwrap();
    assert_eq!(next.number(), 3);
    assert_eq!(next.records().first.locus().seq(), Seq::new(3, 0));
    assert_eq!(next.bytes().start, old_len);
    let seqs = read(&path)
        .unwrap()
        .map(|record| record.unwrap().locus().seq())
        .collect::<Vec<_>>();
    assert_eq!(
        seqs,
        [(1, 0), (2, 0), (2, 1), (2, 2), (3, 0)].map(|(number, index)| Seq::new(number, index))
    );
}

#[test]
fn record_at_reads_each_record_of_a_commit_and_refuses_a_stale_locus() {
    let dir = temp_dir("record-at");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let commit = log.append(several()).unwrap();
    drop(log);
    let mut file = File::open(&path).unwrap();
    for (recorded, expected) in commit.records().iter().zip(["alpha", "bravo", "charlie"]) {
        assert!(
            matches!(Log::record_at(&mut file, recorded.locus()).unwrap(), Record::Forensic(Forensic::Error { text }) if text == expected)
        );
    }
    let mut bytes = terminated(commit_line(&[error("prepended")]));
    bytes.extend(std::fs::read(&path).unwrap());
    std::fs::write(&path, bytes).unwrap();
    let refusal = Log::record_at(&mut file, commit.records().rest[0].locus()).unwrap_err();
    assert!(
        refusal.to_string().contains("did not hash to the locus"),
        "{refusal}"
    );
}
