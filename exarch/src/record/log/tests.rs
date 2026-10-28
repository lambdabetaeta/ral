use super::*;
use crate::record::{Display, Forensic};
use ral_core::own::TestDir;
use std::fs::OpenOptions;

/// A directory of this test's own for its log; held for as long as the test
/// reads the log back.
fn temp_dir(name: &str) -> TestDir {
    OwnDir::for_test(&format!("log-{name}")).expect("a test directory")
}

/// What [`Log::read`] is handed for the record at `path`.
fn read(path: &Path) -> io::Result<impl Iterator<Item = io::Result<Recorded<Record>>> + use<>> {
    Log::read(File::open(path)?, path)
}

#[test]
fn a_record_round_trips_through_the_entry_envelope() {
    let dir = temp_dir("round-trip");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).expect("temp record log");
    let record = Record::Forensic(Forensic::Error {
        text: "boom".into(),
    });
    let _locus = log.append(&record).expect("append");

    let back: Vec<_> = read(&path).expect("read back").collect();
    assert_eq!(back.len(), 1);
    let recorded = back.into_iter().next().unwrap().expect("parses");
    assert!(matches!(
        recorded.into_value(),
        Record::Forensic(Forensic::Error { text }) if text == "boom"
    ));

    let bytes = std::fs::read(&path).unwrap();
    let line = String::from_utf8(bytes).unwrap();
    assert!(
        line.contains("\"at_unix_ms\""),
        "the line on disk must carry the Entry envelope: {line}"
    );
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
    let _locus = log
        .append(&Record::Display(Display::Observation {
            value: observation.clone().encode(),
        }))
        .unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < 1_400_000);
    let records: Vec<_> = read(&path).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(records.len(), 1);
    let Record::Display(Display::Observation { value }) =
        records.into_iter().next().unwrap().into_value()
    else {
        panic!("the observation replays as an observation")
    };
    assert_eq!(Observation::decode(&value).unwrap(), observation);
}

#[test]
fn a_pre_envelope_line_is_refused_by_name() {
    let dir = temp_dir("pre-envelope");
    let path = dir.path().join(RECORD);
    let record = Record::Forensic(Forensic::Error {
        text: "boom".into(),
    });
    let mut line = serde_json::to_vec(&record).unwrap();
    line.push(b'\n');
    std::fs::write(&path, &line).unwrap();

    let back: Vec<_> = read(&path).expect("the file itself reads back").collect();
    assert_eq!(back.len(), 1);
    let error = back.into_iter().next().unwrap().expect_err("no envelope");
    let text = error.to_string();
    assert!(text.contains(&ral_core::path::shown(&path)), "{text}");
    assert!(text.contains("older exarch"), "{text}");
}

#[test]
fn a_locus_round_trips_identically_through_append_and_read() {
    let dir = temp_dir("locus-round-trip");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).expect("temp record log");
    let first = log
        .append(&Record::Forensic(Forensic::Error {
            text: "first".into(),
        }))
        .expect("append");
    let second = log
        .append(&Record::Forensic(Forensic::Error {
            text: "second".into(),
        }))
        .expect("append");

    let back: Vec<_> = read(&path).expect("read back").collect();
    assert_eq!(back.len(), 2);
    let mut back = back.into_iter();
    let first_back = back.next().unwrap().expect("parses");
    let second_back = back.next().unwrap().expect("parses");
    assert_eq!(*first_back.locus(), first);
    assert_eq!(*second_back.locus(), second);
}

#[test]
fn a_mismatched_body_misses_the_digest() {
    let dir = temp_dir("digest-mismatch");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).expect("temp record log");
    let _locus = log
        .append(&Record::Forensic(Forensic::Error {
            text: "boom".into(),
        }))
        .expect("append");

    let recorded = read(&path)
        .expect("read back")
        .next()
        .expect("one record")
        .expect("parses");
    assert_ne!(
        Locus::digest_of(b"something else"),
        recorded.locus().digest()
    );
}

#[test]
fn complete_malformed_lines_are_named_refusals_not_crash_tails() {
    for interior in [false, true] {
        let dir = temp_dir(&format!("complete-garbage-{interior}"));
        let path = dir.path().join(RECORD);
        let log = Log::create(&dir).unwrap();
        let record = Record::Forensic(Forensic::Cancelled);
        let committed = log.append(&record).unwrap();
        drop(log);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"garbage\n").unwrap();
        if interior {
            file.write_all(&crate::record::envelope_line(&record))
                .unwrap();
            file.write_all(b"\n").unwrap();
        }
        drop(file);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            Log::complete_len(&mut File::open(&path).unwrap()).unwrap(),
            before.len() as u64
        );
        let mut records = read(&path).unwrap();
        assert_eq!(records.next().unwrap().unwrap().locus(), &committed);
        let error = records.next().unwrap().unwrap_err().to_string();
        assert!(error.contains("line 2"), "{error}");
        assert!(error.contains(&ral_core::path::shown(&path)), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
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
            let error = log
                .append(&Record::Forensic(Forensic::Cancelled))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::StorageFull);
            assert!(!log.inner.is_poisoned());
        }
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = log.inner.lock_strict().unwrap();
            panic!("a torn append critical section")
        }));
        assert!(panic.is_err());
        let failure = log.check_available().unwrap_err();
        let error = failure.to_string();
        if failed {
            assert_eq!(failure.kind(), io::ErrorKind::StorageFull);
            assert!(error.contains("injected record write failure"), "{error}");
        } else {
            assert!(error.contains("panicked"), "{error}");
        }
        assert_eq!(
            log.append(&Record::Forensic(Forensic::Cancelled))
                .unwrap_err()
                .to_string(),
            error
        );
        assert_eq!(std::fs::read(path).unwrap(), [] as [u8; 0]);
    }
}

#[test]
fn read_ignores_an_unterminated_tail() {
    let dir = temp_dir("torn-tail");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).expect("temp record log");
    let _locus = log
        .append(&Record::Forensic(Forensic::Error {
            text: "whole".into(),
        }))
        .expect("append");
    let whole = std::fs::metadata(&path).unwrap().len();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(br#"{"at_unix_ms":0,"record":{"Forensic":{"kind":"er"#)
        .unwrap();

    assert_eq!(
        Log::complete_len(&mut File::open(&path).unwrap()).unwrap(),
        whole
    );
    let back: Vec<_> = read(&path).expect("read back").collect();
    assert_eq!(back.len(), 1, "the torn tail is never parsed");
    assert!(back.into_iter().next().unwrap().is_ok());
}
