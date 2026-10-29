use super::*;
use crate::bus::{FleetSink, Signal, UsageMeter, channel};
use crate::record::{AgentId, Display, Forensic, Locus, RECORD, log::Log, model};
use ral_core::process::DurableRoot;
use std::fs;

fn opening(dir: &OwnDir, id: AgentId) -> Forensic {
    Forensic::SessionStarted {
        session_id: id,
        name: "main".into(),
        parent: None,
        model: "model".into(),
        context_window: None,
        label: "provider".into(),
        service: "provider".into(),
        account: "provider".into(),
        log_dir: dir.path().to_path_buf(),
        at_unix_ms: 0,
    }
}

fn assert_locus(bytes: &[u8], locus: &Locus) {
    let range = locus.bytes();
    let start = usize::try_from(range.start).unwrap();
    let end = usize::try_from(range.end).unwrap();
    let line = &bytes[start..end];
    assert_eq!(line.last(), Some(&b'\n'));
    let (sum, _) = crate::record::parse_line(&line[..line.len() - 1]).unwrap();
    assert_eq!(sum, locus.sum());
}

#[test]
fn a_commit_is_published_as_one_signal() {
    let dir = OwnDir::for_test("commit-one-signal").unwrap();
    let emit = Emitter::create(&dir).unwrap();
    let id = AgentId::new(7);
    let (tx, rx) = channel();
    emit.attach(Box::new(FleetSink {
        id,
        tx: tx.downgrade(),
        meter: UsageMeter::default(),
    }));
    let returned = emit
        .commit(NonEmpty {
            first: Forensic::Error {
                text: "first".into(),
            }
            .into(),
            rest: vec![
                Forensic::Error {
                    text: "second".into(),
                }
                .into(),
                Forensic::Error {
                    text: "third".into(),
                }
                .into(),
            ],
        })
        .unwrap();
    let Signal::Commit(source, published) = rx.try_recv().unwrap() else {
        panic!("one whole commit is published")
    };
    assert_eq!(source, id);
    let loci = |commit: &Commit| {
        commit
            .records()
            .iter()
            .map(|rec| rec.locus().clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(loci(&published), loci(&returned));
    assert_eq!(published.records().iter().count(), 3);
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn a_failed_writer_preserves_its_commits_until_explicit_resume() {
    for allowed in [0, 17, 16 * 1024] {
        let sessions = OwnDir::for_test(&format!("writer-recovery-{allowed}")).unwrap();
        let id = AgentId::new(7);
        let dir = sessions.make_dir(&id.to_string()).unwrap();
        let path = dir.path().join(RECORD);
        let emit = Emitter::create(&dir).unwrap();
        let first = emit.clone();
        let second = emit.clone();
        let (tx, rx) = channel();
        emit.attach(Box::new(FleetSink {
            id,
            tx: tx.downgrade(),
            meter: UsageMeter::default(),
        }));
        emit.check_available().unwrap();
        let head = emit
            .commit(NonEmpty::one(opening(&dir, id).into()))
            .unwrap();
        let retained = first
            .commit(NonEmpty::one(
                Display::Answer {
                    text: "retained".into(),
                }
                .into(),
            ))
            .unwrap();
        let committed = fs::read(&path).unwrap();
        assert_eq!(
            retained.records().first.locus().bytes().end,
            committed.len() as u64
        );
        let cursor = Cursor::start().past(&retained);

        second.fail_after(allowed);
        let error = second
            .emit(Display::Answer {
                text: "x".repeat(64 * 1024),
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        let detail = error.to_string();
        assert!(detail.contains("injected record write failure"), "{detail}");
        assert!(detail.contains(&ral_core::path::shown(&path)), "{detail}");
        assert!(detail.contains("resume"), "{detail}");
        let failed = fs::read(&path).unwrap();
        assert_eq!(&failed[..committed.len()], committed);
        assert_eq!(failed.len(), committed.len() + allowed);
        assert_eq!(failed.last() == Some(&b'\n'), allowed == 0);

        second.fail_after(usize::MAX);
        for producer in [&emit, &first, &second] {
            let unavailable = producer.check_available().unwrap_err();
            assert_eq!(unavailable.kind(), error.kind());
            assert_eq!(unavailable.to_string(), detail);
            let unavailable = producer.emit(Forensic::Cancelled).unwrap_err();
            assert_eq!(unavailable.kind(), error.kind());
            assert_eq!(unavailable.to_string(), detail);
        }
        let producer = first.clone();
        let unavailable = std::thread::spawn(move || producer.emit(Forensic::SessionEnded))
            .join()
            .unwrap()
            .unwrap_err();
        assert_eq!(unavailable.to_string(), detail);
        assert_eq!(fs::read(&path).unwrap(), failed);

        let (from, records) = emit.reader().since(None).unwrap();
        assert_eq!(from, Cursor::start());
        let records = records.collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(records.len(), 2);
        for (record, expected) in records.iter().zip([
            head.records().first.locus(),
            retained.records().first.locus(),
        ]) {
            assert_eq!(record.records().first.locus(), expected);
            assert_locus(&failed, record.records().first.locus());
        }
        let (from, records) = emit.reader().since(Some(cursor)).unwrap();
        assert_eq!(from, cursor);
        assert_eq!(from.next(), 3);
        assert_eq!(records.count(), 0);

        for expected in [
            head.records().first.locus(),
            retained.records().first.locus(),
        ] {
            let Signal::Commit(source, commit) = rx.recv().unwrap() else {
                panic!("only committed facts are published")
            };
            assert_eq!(source, id);
            assert_eq!(commit.records().first.locus(), expected);
        }
        assert!(rx.try_recv().is_err());

        drop((emit, first, second));
        assert_eq!(fs::read(&path).unwrap(), failed);
        let (context, _) = model::resume(&sessions, id).unwrap();
        assert!(context.is_ready());
        assert_eq!(fs::read(&path).unwrap(), committed);
        let crash = dir.path().join("record.jsonl.crash");
        if allowed == 0 {
            assert!(!crash.exists());
        } else {
            assert_eq!(fs::read(crash).unwrap(), failed[committed.len()..]);
        }

        let reopened = Emitter::append_to(&dir).unwrap();
        reopened.check_available().unwrap();
        let next = reopened
            .emit(Display::Answer {
                text: "after recovery".into(),
            })
            .unwrap();
        assert_eq!(next.locus().seq().commit(), cursor.next());
        assert_eq!(next.locus().bytes().start, committed.len() as u64);
        let recovered = fs::read(&path).unwrap();
        assert_eq!(next.locus().bytes().end, recovered.len() as u64);
        assert_locus(&recovered, next.locus());
        let replayed = Log::read(dir.open(RECORD).unwrap(), &path)
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(replayed.len(), 3);
        assert_eq!(replayed[0].locus(), head.records().first.locus());
        assert_eq!(replayed[1].locus(), retained.records().first.locus());
        assert_eq!(replayed[2].locus(), next.locus());
    }
}

#[test]
fn a_torn_last_commit_is_quarantined_whole() {
    for cut in [true, false] {
        let sessions = OwnDir::for_test(&format!("torn-commit-{cut}")).unwrap();
        let id = AgentId::new(7);
        let dir = sessions.make_dir(&id.to_string()).unwrap();
        let path = dir.path().join(RECORD);
        let emit = Emitter::create(&dir).unwrap();
        let head = emit.emit(opening(&dir, id)).unwrap();
        let start = usize::try_from(head.locus().bytes().end).unwrap();
        let _commit = emit
            .commit(NonEmpty {
                first: Forensic::Error {
                    text: "first record".into(),
                }
                .into(),
                rest: vec![
                    Forensic::Error {
                        text: "second record".into(),
                    }
                    .into(),
                ],
            })
            .unwrap();
        drop(emit);
        let mut bytes = fs::read(&path).unwrap();
        if cut {
            bytes.truncate(start + (bytes.len() - start) / 2);
        } else {
            let at = bytes
                .windows(13)
                .position(|text| text == b"second record")
                .unwrap();
            bytes[at] = b'S';
        }
        fs::write(&path, &bytes).unwrap();
        let (_context, _selection) = model::resume(&sessions, id).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes[..start]);
        assert_eq!(
            fs::read(dir.path().join("record.jsonl.crash")).unwrap(),
            bytes[start..]
        );
        let back = Log::read(dir.open(RECORD).unwrap(), &path)
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].locus(), head.locus());
    }
}

#[test]
fn a_refused_resume_leaves_the_record_as_it_found_it() {
    let sessions = OwnDir::for_test("refused-resume-unchanged").unwrap();
    let id = AgentId::new(7);
    let dir = sessions.make_dir(&id.to_string()).unwrap();
    let path = dir.path().join(RECORD);
    let emit = Emitter::create(&dir).unwrap();
    let _head = emit.emit(opening(&dir, id)).unwrap();
    let _a = emit
        .commit(NonEmpty {
            first: Forensic::Error {
                text: "first record".into(),
            }
            .into(),
            rest: vec![
                Forensic::Error {
                    text: "second record".into(),
                }
                .into(),
            ],
        })
        .unwrap();
    let b = emit
        .commit(NonEmpty::one(Forensic::Cancelled.into()))
        .unwrap();
    drop(emit);
    let mut bytes = fs::read(&path).unwrap();
    let at = bytes
        .windows(13)
        .position(|text| text == b"second record")
        .unwrap();
    bytes[at] = b'S';
    let start = usize::try_from(b.bytes().start).unwrap();
    bytes.truncate(start + (bytes.len() - start) / 2);
    fs::write(&path, &bytes).unwrap();
    let refusal = model::resume(&sessions, id)
        .err()
        .expect("corrupt interior refused")
        .to_string();
    assert!(refusal.contains(&ral_core::path::shown(&path)), "{refusal}");
    assert!(refusal.contains("line 2"), "{refusal}");
    assert!(refusal.contains("corrupt"), "{refusal}");
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(!dir.path().join("record.jsonl.crash").exists());
}

#[test]
fn a_log_of_an_older_exarch_is_refused_by_name() {
    let sessions = OwnDir::for_test("older-resume").unwrap();
    let id = AgentId::new(7);
    let dir = sessions.make_dir(&id.to_string()).unwrap();
    let path = dir.path().join(RECORD);
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "at_unix_ms": 0, "record": Record::Forensic(opening(&dir, id))
    }))
    .unwrap();
    bytes.push(b'\n');
    fs::write(&path, &bytes).unwrap();
    let refusal = model::resume(&sessions, id)
        .err()
        .expect("older log refused")
        .to_string();
    assert!(refusal.contains(&ral_core::path::shown(&path)), "{refusal}");
    assert!(refusal.contains("line 1"), "{refusal}");
    assert!(refusal.contains("older exarch"), "{refusal}");
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(!dir.path().join("record.jsonl.crash").exists());
}

#[test]
fn availability_observation_does_not_wait_for_publication() {
    use std::sync::{Weak, mpsc};
    use std::time::Duration;

    struct Probe {
        log: Weak<Log>,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Publish for Probe {
        fn commit(&self, _: Commit) {
            self.entered.send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(5))
                .expect("availability must finish while publication holds the append lock");
            Emitter {
                log: self.log.upgrade().unwrap(),
            }
            .check_available()
            .expect("a publisher can also observe availability directly");
        }

        fn transient(&self, _: Transient) {
            panic!("this probe expects only a committed fact")
        }
    }

    let dir = OwnDir::for_test("availability-during-publication").unwrap();
    let emit = Emitter::create(&dir).unwrap();
    let (entered, publishing) = mpsc::channel();
    let (release, observe) = mpsc::channel();
    emit.attach(Box::new(Probe {
        log: Arc::downgrade(&emit.log),
        entered,
        release: observe,
    }));
    let producer = emit.clone();
    let writer = std::thread::spawn(move || producer.emit(Forensic::Cancelled));
    publishing
        .recv_timeout(Duration::from_secs(5))
        .expect("the publisher holds the append lock");
    emit.check_available()
        .expect("availability must not wait for the publisher");
    release.send(()).unwrap();
    let fact = writer.join().unwrap().unwrap();
    let bytes = fs::read(dir.path().join(RECORD)).unwrap();
    assert_locus(&bytes, fact.locus());
    emit.check_available().unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn a_serialization_failure_latches_the_writer_without_writing() {
    let dir = OwnDir::for_test("serialization-before-write").unwrap();
    let path = dir.path().join(RECORD);
    let emit = Emitter::create(&dir).unwrap();
    let mut record = opening(&dir, AgentId::new(7));
    let Forensic::SessionStarted { log_dir, .. } = &mut record else {
        panic!("the opening record carries a path")
    };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        *log_dir = std::ffi::OsString::from_vec(vec![0xff]).into();
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt as _;
        *log_dir = std::ffi::OsString::from_wide(&[0xd800]).into();
    }

    let failure = emit.emit(record).unwrap_err();
    assert_eq!(
        emit.check_available().unwrap_err().to_string(),
        failure.to_string()
    );
    assert_eq!(fs::read(&path).unwrap(), [] as [u8; 0]);
    let (_, records) = emit.reader().since(None).unwrap();
    assert_eq!(records.count(), 0);
    let refused = emit.emit(Forensic::Cancelled).unwrap_err();
    assert_eq!(refused.kind(), failure.kind());
    assert_eq!(refused.to_string(), failure.to_string());
    assert_eq!(fs::read(&path).unwrap(), [] as [u8; 0]);
}

#[test]
fn a_failure_ends_the_life_bound_to_its_writer() {
    let dir = OwnDir::for_test("failure-ends-life").unwrap();
    let emit = Emitter::create(&dir).unwrap();
    let node = DurableRoot::default().child();
    emit.bind(node.clone());
    emit.fail_after(0);
    assert!(emit.emit(Forensic::Cancelled).is_err());
    assert!(node.is_cancelled());

    let dir = OwnDir::for_test("bind-after-failure").unwrap();
    let failed = Emitter::create(&dir).unwrap();
    failed.fail_after(0);
    assert!(failed.emit(Forensic::Cancelled).is_err());
    let later = DurableRoot::default().child();
    failed.bind(later.clone());
    assert!(later.is_cancelled());

    let dir = OwnDir::for_test("poison-ends-life").unwrap();
    let emit = Emitter::create(&dir).unwrap();
    let node = DurableRoot::default().child();
    emit.bind(node.clone());
    emit.poison_append();
    assert!(!node.is_cancelled());
    let failure = emit.check_available().unwrap_err();
    assert!(failure.to_string().contains("panicked"), "{failure}");
    assert!(node.is_cancelled());
}

#[test]
fn a_writer_fault_does_not_disable_an_independent_session() {
    let broken_dir = OwnDir::for_test("broken-writer").unwrap();
    let healthy_dir = OwnDir::for_test("healthy-writer").unwrap();
    let broken = Emitter::create(&broken_dir).unwrap();
    let healthy = Emitter::create(&healthy_dir).unwrap();
    broken.fail_after(0);
    assert!(broken.emit(Forensic::Cancelled).is_err());
    healthy.check_available().unwrap();
    let fact = healthy.emit(Forensic::Cancelled).unwrap();
    assert_eq!(fact.locus().seq().commit(), 1);
    let bytes = fs::read(healthy_dir.path().join(RECORD)).unwrap();
    assert_eq!(fact.locus().bytes(), 0..bytes.len() as u64);
    assert_locus(&bytes, fact.locus());
    assert_eq!(
        fs::read(broken_dir.path().join(RECORD)).unwrap(),
        [] as [u8; 0]
    );
}
