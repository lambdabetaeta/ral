use super::*;
use crate::record::{ChildStatus, Control, Display, Forensic, Seq};
use ral_core::data::FOValue;
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

/// `line` with its sum's digits overwritten by a non-hex letter.
fn unsummed(mut line: Vec<u8>) -> Vec<u8> {
    let at = b"{\"sum\":\"".len();
    line[at..at + 16].fill(b'g');
    line
}

/// A well-formed line of a shape this exarch never wrote, sealed or not.
fn foreign_lines() -> [Vec<u8>; 3] {
    let records = serde_json::json!([error("x")]);
    let body = serde_json::json!({"at_unix_ms": 0, "records": records}).to_string();
    let positional = serde_json::json!([0, records]).to_string();
    [
        serde_json::json!({"at_unix_ms": 0, "record": error("old")}).to_string(),
        format!(r#"["{}",{body}]"#, Sum::of(body.as_bytes())),
        format!(
            r#"{{"sum":"{}","commit":{positional}}}"#,
            Sum::of(positional.as_bytes())
        ),
    ]
    .map(String::into_bytes)
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
        ral_core::test_helper::site(),
        None,
        Observed::Command(Command::new(
            "producer",
            [],
            0,
            CommandOrigin::External,
            AuditIo {
                stdout: ral_core::Retained {
                    bytes: bytes.clone(),
                    total: 2 * bytes.len() as u64,
                },
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

/// `f` on a thread of Rust's default spawned stack, 2 MiB, which no append or
/// read may outgrow.
fn on_a_default_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn_scoped(scope, f)
            .unwrap()
            .join()
            .unwrap()
    })
}

#[test]
fn a_deeply_nested_record_round_trips() {
    // Building, comparing and dropping a 5000-deep value recurses natively.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(deep_record_round_trips)
        .unwrap()
        .join()
        .unwrap();
}

fn deep_record_round_trips() {
    let replied = || {
        (0..5000).fold(FOValue::Unit, |inner, _| FOValue::List {
            items: vec![inner],
        })
    };
    let dir = temp_dir("deep");
    let path = dir.path().join(RECORD);
    let record = Record::Control(Control::Status {
        status: ChildStatus::Replied(replied()),
    });
    let commit =
        on_a_default_stack(|| Log::create(&dir).unwrap().append(NonEmpty::one(record))).unwrap();
    let (back, line) = on_a_default_stack(|| {
        let back = read(&path)
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut reader = LocusReader::new(File::open(&path).unwrap());
        let _record = reader.record(commit.records().first.locus()).unwrap();
        (back, reader.line.take().unwrap())
    });
    for record in [back[0].value(), line.records().first.value()] {
        assert!(matches!(
            record,
            Record::Control(Control::Status { status: ChildStatus::Replied(got) }) if *got == replied()
        ));
    }
}

#[test]
fn a_line_of_an_older_exarch_is_refused_by_name() {
    for (index, (line, interior)) in foreign_lines()
        .into_iter()
        .flat_map(|line| [(line.clone(), false), (line, true)])
        .enumerate()
    {
        let dir = temp_dir(&format!("older-{index}"));
        let path = dir.path().join(RECORD);
        let mut bytes = terminated(line);
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
fn a_torn_last_line_is_never_read() {
    let whole = terminated(commit_line(&several().into_iter().collect::<Vec<_>>()));
    let tails = [
        whole[..whole.len() / 2].to_vec(),
        unsummed(whole.clone()),
        changed(whole),
        b"garbage\n".to_vec(),
        b"{\"unexpected\":\n".to_vec(),
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
fn every_byte_after_the_first_damage_is_torn() {
    let faults = [
        changed(terminated(commit_line(&[error("bravo")]))),
        unsummed(terminated(commit_line(&[error("bravo")]))),
        b"garbage\n".to_vec(),
        b"{\"unexpected\":\n".to_vec(),
        vec![0; 8192],
    ];
    for (index, fault) in faults.into_iter().enumerate() {
        let dir = temp_dir(&format!("interior-{index}"));
        let path = dir.path().join(RECORD);
        let head = terminated(commit_line(&[error("head")]));
        let mut bytes = head.clone();
        bytes.extend(fault);
        bytes.extend(terminated(commit_line(&[error("last")])));
        bytes.extend(b"{\"not\":\"a commit\"}\n");
        bytes.extend(b"more damage\n");
        bytes.extend(terminated(commit_line(&[error("later")])));
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
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn a_foreign_line_before_damage_is_still_refused() {
    let dir = temp_dir("foreign-before-damage");
    let path = dir.path().join(RECORD);
    let mut bytes = terminated(commit_line(&[error("head")]));
    bytes.extend(b"{\"not\":\"a commit\"}\n");
    bytes.extend(b"damaged\n");
    bytes.extend(terminated(commit_line(&[error("last")])));
    std::fs::write(&path, &bytes).unwrap();
    let mut back = read(&path).unwrap();
    assert!(back.next().unwrap().is_ok());
    assert_refusal(&back.next().unwrap().unwrap_err(), &path, 2, "older exarch");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
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
            "expected a length of at least 1, got 0",
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
fn reopening_for_append_refuses_a_torn_tail() {
    let whole = terminated(commit_line(&[error("bravo")]));
    for (index, tail) in [whole[..whole.len() / 2].to_vec(), changed(whole)]
        .into_iter()
        .enumerate()
    {
        let dir = temp_dir(&format!("reopen-torn-{index}"));
        let path = dir.path().join(RECORD);
        let mut bytes = terminated(commit_line(&[error("head")]));
        bytes.extend(tail);
        std::fs::write(&path, &bytes).unwrap();
        let refusal = Log::append_to(&dir).err().expect("a torn tail is refused");
        let text = refusal.to_string();
        assert!(text.contains(&ral_core::path::shown(&path)), "{text}");
        assert!(text.contains("torn tail"), "{text}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    let dir = temp_dir("reopen-empty");
    std::fs::write(dir.path().join(RECORD), b"").unwrap();
    let log = Log::append_to(&dir).unwrap();
    assert_eq!(
        log.append(NonEmpty::one(error("first"))).unwrap().number(),
        1
    );
}

#[test]
fn a_reader_reads_each_record_of_each_commit_and_refuses_a_stale_locus() {
    let dir = temp_dir("reader");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let commit = log.append(several()).unwrap();
    let next = log.append(NonEmpty::one(error("delta"))).unwrap();
    drop(log);
    let mut reader = LocusReader::new(File::open(&path).unwrap());
    let loci = commit
        .records()
        .iter()
        .chain(next.records())
        .chain(commit.records().rest.iter().rev());
    for (recorded, expected) in loci.zip(["alpha", "bravo", "charlie", "delta", "charlie", "bravo"])
    {
        assert!(
            matches!(reader.record(recorded.locus()).unwrap(), Record::Forensic(Forensic::Error { text }) if text == expected)
        );
    }
    let beyond = Locus {
        seq: Seq::new(1, 3),
        ..commit.records().first.locus().clone()
    };
    let refusal = reader.record(&beyond).unwrap_err();
    assert_eq!(
        refusal,
        RecordReadFailure::MissingLocusRecord(RecordReadFailureMissingLocusRecord {
            commit: 1u64.into(),
            index: 3usize.into(),
        })
    );
    let mut bytes = terminated(commit_line(&[error("prepended")]));
    bytes.extend(std::fs::read(&path).unwrap());
    std::fs::write(&path, bytes).unwrap();
    let refusal = LocusReader::new(File::open(&path).unwrap())
        .record(commit.records().rest[0].locus())
        .unwrap_err();
    assert_eq!(
        refusal,
        RecordReadFailure::LocusSealMismatch(RecordReadFailureLocusSealMismatch {
            commit: commit.number().into(),
        })
    );
}

#[test]
fn a_reader_refuses_a_stale_range_before_parsing_its_records() {
    let dir = temp_dir("reader-stale-unparsed");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let commit = log.append(several()).unwrap();
    drop(log);
    let line = raw::sealed_fixture(br#"{"at_unix_ms":0,"records":[{"alien":1}]}"#);
    std::fs::write(&path, terminated(line)).unwrap();
    let refusal = LocusReader::new(File::open(&path).unwrap())
        .record(commit.records().first.locus())
        .unwrap_err();
    assert_eq!(
        refusal,
        RecordReadFailure::LocusSealMismatch(RecordReadFailureLocusSealMismatch {
            commit: commit.number().into(),
        })
    );
}

#[test]
fn sync_does_not_hold_the_append_lock_and_certifies_only_its_noted_prefix() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = temp_dir("sync-prefix");
    let log = Arc::new(Log::create(&dir).unwrap());
    let first = log.append(NonEmpty::one(error("first"))).unwrap();
    let (entered, syncing) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    log.before_sync(move || {
        entered.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5))
            .expect("append is not blocked by sync");
        Ok(())
    });
    let writer = Arc::clone(&log);
    let thread = std::thread::spawn(move || writer.sync());
    syncing.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = log.append(NonEmpty::one(error("second"))).unwrap();
    log.publish_transient(Transient::Boundary);
    assert_eq!(log.inner.lock_strict().unwrap().synced, 0);
    release.send(()).unwrap();
    thread.join().unwrap().unwrap();
    log.assert_synced_through(first.number());
    assert_eq!(log.inner.lock_strict().unwrap().synced, first.number());
    log.sync().unwrap();
    log.assert_synced_through(second.number());
    log.before_sync(|| panic!("an adjacent sync of the same prefix must coalesce"));
    log.sync().unwrap();
}

#[test]
fn concurrent_syncs_share_the_first_failure_even_while_another_thread_appends() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = temp_dir("concurrent-sync-failure");
    let log = Arc::new(Log::create(&dir).unwrap());
    let _first = log.append(NonEmpty::one(error("first"))).unwrap();
    let (entered, syncing) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    log.before_sync(move || {
        entered.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        Err(io::Error::other("injected one-shot write-back failure"))
    });
    let writer = Arc::clone(&log);
    let first = std::thread::spawn(move || writer.sync());
    syncing.recv_timeout(Duration::from_secs(5)).unwrap();
    let writer = Arc::clone(&log);
    let second = std::thread::spawn(move || writer.sync());
    let writer = Arc::clone(&log);
    let append = std::thread::spawn(move || writer.append(NonEmpty::one(error("during sync"))));
    drop(append.join().unwrap().unwrap());
    release.send(()).unwrap();
    let failure = first.join().unwrap().unwrap_err();
    let concurrent = second.join().unwrap().unwrap_err();
    assert!(Arc::ptr_eq(&failure.0, &concurrent.0));
    assert!(failure.to_string().contains("syncing"));
    assert!(
        failure
            .to_string()
            .contains("injected one-shot write-back failure")
    );
    assert_eq!(log.inner.lock_strict().unwrap().synced, 0);
    assert!(Arc::ptr_eq(&failure.0, &log.sync().unwrap_err().0));
    assert!(Arc::ptr_eq(
        &failure.0,
        &log.append(NonEmpty::one(error("too late"))).unwrap_err().0
    ));
}

#[test]
fn a_write_failure_during_sync_prevents_watermark_publication() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = temp_dir("write-during-sync");
    let log = Arc::new(Log::create(&dir).unwrap());
    let _first = log.append(NonEmpty::one(error("first"))).unwrap();
    let (entered, syncing) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    log.before_sync(move || {
        entered.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(())
    });
    let writer = Arc::clone(&log);
    let sync = std::thread::spawn(move || writer.sync());
    syncing.recv_timeout(Duration::from_secs(5)).unwrap();
    log.fail_after(0);
    let failed = log.append(NonEmpty::one(error("fails"))).unwrap_err();
    release.send(()).unwrap();
    assert!(Arc::ptr_eq(&failed.0, &sync.join().unwrap().unwrap_err().0));
    assert_eq!(log.inner.lock_strict().unwrap().synced, 0);
}

fn line_fault(bytes: &[u8]) -> Unsealed {
    match unseal(bytes) {
        Err(fault) => fault,
        Ok(line) => records_of(line.body()).unwrap_err(),
    }
}

#[test]
fn log_checksum_uses_original_commit_ranges_not_raw_reencoding() {
    let body = br#" { "records" : [ {"Forensic":{"error":{"text":"\u0062ravo"}}} ], "at_unix_ms" : 18446744073709551615 } "#;
    // Surrounding whitespace is outside the JSON value, hence outside its range.
    let body = &body[1..body.len() - 1];
    let bytes = raw::sealed_fixture(body);
    let line = unseal(&bytes).unwrap();
    assert_eq!(line.body().source(), body);
    assert_eq!(line.sum, Sum::of(body));
    let records = records_of(line.body()).unwrap();
    let raw = crate::record::json::RawBody::from_records(u64::MAX, &records).unwrap();
    let normalized = raw.encode(ral_core::data::json::JsonFormat::Compact);
    assert_ne!(normalized, body);
    assert_ne!(Sum::of(&normalized), line.sum);
    assert!(
        matches!(&records.first, Record::Forensic(Forensic::Error { text }) if text == "bravo")
    );
}

#[test]
fn log_unsealed_projection_preserves_syntax_envelope_seal_records_precedence() {
    use ral_core::fact::{DataStep, DecodeCause, JsonShape};

    let fault = line_fault(br#"{"sum":false,"commit":[}"#);
    assert!(fault.damaged());
    assert!(matches!(fault.failure(), RecordLineFailure::JsonSyntax(_)));

    let fault = line_fault(br#"{"sum":"not-a-seal","commit":false,"extra":0}"#);
    assert!(!fault.damaged());
    assert!(
        matches!(fault.failure(), RecordLineFailure::InvalidEnvelope(cause)
        if matches!(cause.cause.as_ref(), DecodeCause::UnknownField(field) if field.field == "extra"))
    );

    let fault = line_fault(br#"{"sum":false,"commit":false}"#);
    assert!(!fault.damaged());
    assert!(
        matches!(fault.failure(), RecordLineFailure::InvalidEnvelope(cause)
        if cause.path == vec![DataStep::Field("sum".into())]
        && matches!(cause.cause.as_ref(), DecodeCause::JsonShape(shape) if shape.found == JsonShape::Bool))
    );

    for sealed in ["not-a-seal", "FFFFFFFFFFFFFFFF", "0000000000000000"] {
        let bytes = format!(r#"{{"sum":"{sealed}","commit":false}}"#);
        let fault = line_fault(bytes.as_bytes());
        assert!(fault.damaged());
        assert_eq!(
            fault.failure(),
            RecordLineFailure::SealMismatch(RecordLineFailureSealMismatch {
                sealed: sealed.into(),
                actual: Sum::of(b"false").to_string(),
            })
        );
    }

    let fault = line_fault(&raw::sealed_fixture(b"false"));
    assert!(!fault.damaged());
    assert!(
        matches!(fault.failure(), RecordLineFailure::InvalidRecords(cause)
        if cause.path.is_empty() && matches!(cause.cause.as_ref(), DecodeCause::JsonShape(_)))
    );
}

#[test]
fn locus_reader_reports_typed_records_only_after_both_seals_agree() {
    let dir = temp_dir("reader-typed-records");
    let path = dir.path().join(RECORD);
    let bytes = raw::sealed_fixture(br#"{"at_unix_ms":0,"records":[]}"#);
    let sum = unseal(&bytes).unwrap().sum;
    let expected = line_fault(&bytes).failure();
    let bytes = terminated(bytes);
    let locus = Locus {
        seq: Seq::new(7, 0),
        bytes: 0..bytes.len() as u64,
        sum,
    };
    std::fs::write(&path, bytes).unwrap();
    let mut reader = LocusReader::new(File::open(&path).unwrap());
    assert_eq!(
        reader.record(&locus).unwrap_err(),
        RecordReadFailure::Line(RecordReadFailureLine {
            commit: 7u64.into(),
            cause: expected,
        })
    );
    let stale = Locus {
        sum: Sum::of(b"stale"),
        ..locus
    };
    assert_eq!(
        reader.record(&stale).unwrap_err(),
        RecordReadFailure::LocusSealMismatch(RecordReadFailureLocusSealMismatch {
            commit: 7u64.into(),
        })
    );
}

#[cfg(unix)]
#[test]
fn log_encoding_failure_is_typed_latched_and_never_appended_or_published() {
    use crate::provider::Selected;
    use crate::record::{AgentId, Started};
    use ral_core::fact::{DataStep, IoLeaf, JsonEncodeCause};
    use std::os::unix::ffi::OsStringExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Sink(Arc<AtomicUsize>);
    impl Publish for Sink {
        fn commit(&self, _: Commit) {
            let _ = self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn synced(&self, _: u64) {}
        fn transient(&self, _: Transient) {}
    }
    let dir = temp_dir("encoding-failure");
    let path = dir.path().join(RECORD);
    let log = Log::create(&dir).unwrap();
    let published = Arc::new(AtomicUsize::new(0));
    log.attach(Box::new(Sink(Arc::clone(&published))));
    let first = log.append(NonEmpty::one(error("prefix"))).unwrap();
    let prefix = std::fs::read(&path).unwrap();
    let started = Started {
        session_id: AgentId::new(1),
        name: "agent".into(),
        parent: None,
        returns: false,
        selected: Selected::for_test("fixture-model"),
        log_dir: std::ffi::OsString::from_vec(vec![b'/', 0xff]).into(),
        at_unix_ms: 0,
    };
    let failure = log
        .append(NonEmpty::one(Record::Control(Control::SessionStarted(
            started,
        ))))
        .unwrap_err();
    let RecordingFailure::Recording(recording) = failure.failure() else {
        panic!("append failure")
    };
    assert_eq!(recording.stage, RecordingStage::Append);
    let IoLeaf::JsonEncoding(cause) = recording.cause.leaf.as_ref() else {
        panic!("typed encoding cause")
    };
    assert_eq!(
        cause.path,
        vec![
            DataStep::Field("records".into()),
            DataStep::Index(0u64.into()),
            DataStep::Payload("Control".into()),
            DataStep::Payload("session_started".into()),
            DataStep::Field("log_dir".into())
        ]
    );
    assert!(matches!(cause.cause, JsonEncodeCause::NonUnicodePath(_)));
    assert_eq!(std::fs::read(&path).unwrap(), prefix);
    assert_eq!(log.inner.lock_strict().unwrap().commits, first.number());
    assert_eq!(published.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(
        &failure.0,
        &log.append(NonEmpty::one(error("later"))).unwrap_err().0
    ));
    assert!(Arc::ptr_eq(&failure.0, &log.sync().unwrap_err().0));
}
