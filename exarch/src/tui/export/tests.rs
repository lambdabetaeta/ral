use super::*;
use crate::record::RECORD;
use crate::record::{
    BlockId, Display, EditAuthority, Emitter as Recorder, Eviction, Span, Transcript, Verdict,
    Voice,
};
use ral_core::data::datum::Datum as _;
use ral_core::fact::Read;
use ral_core::own::OwnDir;
use ral_core::shell::runtime::{Observation, Observed};

fn prompt(turn: u64, text: &str) -> Transcript {
    Transcript::UserPrompt {
        turn,
        text: text.into(),
        voice: Voice::Human,
    }
}

fn rendered(record: &Path) -> String {
    let mut out = Vec::new();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(record)
        .expect("the log opens");
    let snapshot = Snapshot::inactive(file, record, crate::record::AgentId::new(0))
        .expect("validated inactive source");
    export(&snapshot, &mut out, Format::Plain).expect("the log exports");
    String::from_utf8(out).expect("the transcript is text")
}

/// A session that asks, reads, is evicted from, is cleared, and asks again.
fn session(dir: &OwnDir) -> (Recorder, std::path::PathBuf) {
    let path = dir.path().join(RECORD);
    let recorder = Recorder::create(dir).expect("a record log");
    recorder
        .begin(crate::record::Started {
            session_id: crate::record::AgentId::new(0),
            name: "export".into(),
            parent: None,
            returns: false,
            selected: crate::record::Selected::for_test("test"),
            log_dir: dir.path().to_path_buf(),
            at_unix_ms: 0,
        })
        .unwrap();
    let emit = |record: Record| recorder.emit(record).expect("the log appends");
    let _ = emit(prompt(1, "first prompt").into());
    let _ = emit(
        Display::Answer {
            text: "the answer\n".into(),
        }
        .into(),
    );
    let call = emit(
        Display::ToolCall {
            tool: "ral".into(),
            cmd: "read 'a.rs'".into(),
            summary: Some("read a file".into()),
        }
        .into(),
    );
    let _ = emit(
        Display::Observation {
            value: Observation::instant(
                None,
                None,
                Observed::Read(Read {
                    path: "a.rs".into(),
                }),
            )
            .encode(),
        }
        .into(),
    );
    let _ = emit(
        Display::Verdict {
            call: BlockId::new(call.locus().seq()),
            verdict: Verdict::of("a line\n", false),
        }
        .into(),
    );
    let _ = emit(
        Transcript::Evicted(Eviction {
            span: Span::single(1),
            note: None,
            by: EditAuthority::Harness,
        })
        .into(),
    );
    let _ = emit(Transcript::Cleared.into());
    let _ = emit(prompt(2, "second prompt").into());
    (recorder, path)
}

/// A clear drops the screen, not the transcript: everything said before it
/// is exported, then the clear, then what came after.
#[test]
fn a_clear_keeps_the_transcript_before_it() {
    let dir = OwnDir::for_test("export").expect("a test directory");
    let (_recorder, path) = session(&dir);
    let text = rendered(&path);
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from {text:?}"))
    };
    let order = [
        "first prompt",
        "the answer",
        "read a file",
        "left the context",
        "[cleared]",
        "second prompt",
    ];
    assert!(
        order.windows(2).all(|pair| at(pair[0]) < at(pair[1])),
        "the transcript reads in the order it happened: {text:?}"
    );
}

/// A write the process died in the middle of is not a record.
#[test]
fn a_torn_tail_is_ignored() {
    let dir = OwnDir::for_test("export").expect("a test directory");
    let (_recorder, path) = session(&dir);
    let whole = rendered(&path);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("the log reopens");
    file.write_all(br#"{"sum":"0123456789abcdef","commit":{"at_unix_ms":0,"records":[{"Pro"#)
        .expect("a torn write");
    assert_eq!(rendered(&path), whole);
}

#[test]
fn an_inactive_managed_export_holds_the_existing_lock_and_freezes_its_prefix() {
    let root = OwnDir::for_test("managed-export").unwrap();
    let run = root.make_dir("project").unwrap().make_dir("run").unwrap();
    let held = run.make_dir("sessions").unwrap().make_dir("0").unwrap();
    let (recorder, _) = session(&held);
    drop(crate::app::RunLock::try_acquire(&run).unwrap());
    let (snapshot, lock) = inactive_under(&root, &held).unwrap();
    assert_eq!(
        crate::app::RunLock::try_existing(&run)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    let _later = recorder
        .emit(Display::Answer {
            text: "past the frozen prefix\n".into(),
        })
        .unwrap();
    let mut out = Vec::new();
    export(&snapshot, &mut out, Format::Plain).unwrap();
    assert!(
        !String::from_utf8(out)
            .unwrap()
            .contains("past the frozen prefix")
    );
    drop(lock);
    assert!(crate::app::RunLock::try_existing(&run).is_ok());
}

#[test]
fn active_unmanaged_missing_lock_and_mismatched_session_exports_fail_before_destination() {
    let root = OwnDir::for_test("refused-export").unwrap();
    let run = root.make_dir("project").unwrap().make_dir("run").unwrap();
    let held = run.make_dir("sessions").unwrap().make_dir("0").unwrap();
    let (_, _) = session(&held);
    let dest = root.path().join("must-not-exist.txt");
    let attempt = |session: &OwnDir| {
        let (snapshot, _lock) = inactive_under(&root, session)?;
        export_to(&snapshot, &dest, Format::Plain)
    };
    assert!(attempt(&held).is_err());
    assert!(
        run.stat("run.lock").is_none(),
        "export never creates a lock witness"
    );
    let lock = crate::app::RunLock::try_acquire(&run).unwrap();
    let error = attempt(&held).err().unwrap();
    assert!(error.to_string().contains("use /export"));
    assert!(!dest.exists());
    drop(lock);
    let other = root.make_dir("arbitrary").unwrap();
    assert!(
        attempt(&other)
            .err()
            .unwrap()
            .to_string()
            .contains("no owning-run lock")
    );
    let mismatch = run.dir("sessions").unwrap().make_dir("1").unwrap();
    let _ = session(&mismatch);
    assert!(
        attempt(&mismatch)
            .err()
            .unwrap()
            .to_string()
            .contains("session_id")
    );
    assert!(!dest.exists());
}

#[test]
fn inactive_sync_failure_creates_no_destination_writes_no_output_and_releases_the_lock() {
    let root = OwnDir::for_test("export-sync-failed").unwrap();
    let run = root.make_dir("project").unwrap().make_dir("run").unwrap();
    let held = run.make_dir("sessions").unwrap().make_dir("0").unwrap();
    let (_recorder, source) = session(&held);
    drop(crate::app::RunLock::try_acquire(&run).unwrap());
    let before = held.read(RECORD).unwrap();
    let dest = root.path().join("must-not-exist.txt");
    let mut output = Vec::new();
    for to_file in [false, true] {
        Snapshot::fail_next_sync();
        let attempted = inactive_under(&root, &held).and_then(|(snapshot, _lock)| {
            if to_file {
                export_to(&snapshot, &dest, Format::Plain)
            } else {
                export(&snapshot, &mut output, Format::Plain)
            }
        });
        let error = attempted.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected inactive source sync failure")
        );
        assert!(error.to_string().contains(&ral_core::path::shown(&source)));
        assert!(!dest.exists());
        assert_eq!(output, Vec::<u8>::new());
        assert_eq!(held.read(RECORD).unwrap(), before);
        assert!(crate::app::RunLock::try_existing(&run).is_ok());
    }
}

#[test]
fn both_managed_input_forms_resolve_the_same_session_and_refuse_missing_or_nonregular_sources() {
    let root = OwnDir::for_test("export-input-forms").unwrap();
    let run = root.make_dir("project").unwrap().make_dir("run").unwrap();
    let held = run.make_dir("sessions").unwrap().make_dir("0").unwrap();
    drop(crate::app::RunLock::try_acquire(&run).unwrap());
    for target in [held.path().to_path_buf(), held.path().join(RECORD)] {
        let session = root.within(session_target(&target).unwrap()).unwrap();
        assert!(session.same_directory(&held));
        let error = inactive_under(&root, &session).err().unwrap();
        assert!(error.to_string().contains("read/write permission"));
        assert!(
            held.stat(RECORD).is_none(),
            "a missing source is never created"
        );
    }
    held.make_dir(RECORD).unwrap();
    assert!(inactive_under(&root, &held).is_err());
}

#[cfg(unix)]
#[test]
fn inactive_export_refuses_replaced_sessions_before_source_open_including_after_lock() {
    for after_lock in [false, true] {
        let root = OwnDir::for_test("export-session-replaced").unwrap();
        let run = root.make_dir("project").unwrap().make_dir("run").unwrap();
        let sessions = run.make_dir("sessions").unwrap();
        let held = sessions.make_dir("0").unwrap();
        drop(crate::app::RunLock::try_acquire(&run).unwrap());
        let replacement = move || {
            sessions.rename("0", "original").unwrap();
            let other = sessions.make_dir("0").unwrap();
            // Missing source proves association is checked before source opening.
            assert!(other.stat(RECORD).is_none());
        };
        if after_lock {
            AFTER_EXPORT_LOCK.with(|hook| hook.set(Some(Box::new(replacement))));
        } else {
            replacement();
        }
        let error = inactive_under(&root, &held).err().unwrap();
        assert!(
            error
                .to_string()
                .contains("directory changed during resolution"),
            "{error}"
        );
        assert!(
            crate::app::RunLock::try_existing(&run).is_ok(),
            "failure releases its lock"
        );
    }
}

#[cfg(unix)]
#[test]
fn inactive_export_accepts_parent_aliases_but_refuses_record_links_and_special_files() {
    use std::ffi::CString;
    use std::os::unix::{ffi::OsStrExt as _, fs::symlink};
    let root = OwnDir::for_test("export-source-kinds").unwrap();
    let run = root.make_dir("project").unwrap().make_dir("run").unwrap();
    let held = run.make_dir("sessions").unwrap().make_dir("0").unwrap();
    let (_recorder, _) = session(&held);
    drop(crate::app::RunLock::try_acquire(&run).unwrap());
    symlink(held.path(), root.path().join("alias")).unwrap();
    for target in [
        root.path().join("alias"),
        root.path().join("alias").join(RECORD),
    ] {
        let associated = root.within(session_target(&target).unwrap()).unwrap();
        assert!(associated.same_directory(&held));
        assert!(inactive_under(&root, &associated).is_ok());
    }
    held.rename(RECORD, "original.jsonl").unwrap();
    symlink("original.jsonl", held.path().join(RECORD)).unwrap();
    let error = inactive_under(&root, &held).err().unwrap();
    assert!(error.to_string().contains("is a link"), "{error}");
    // Keep the link as evidence; a second session holds a special source.
    let special = run.dir("sessions").unwrap().make_dir("1").unwrap();
    let fifo = CString::new(special.path().join(RECORD).as_os_str().as_bytes()).unwrap();
    // SAFETY: fifo is a live, NUL-terminated path; mkfifo retains no pointer.
    let result = unsafe { libc::mkfifo(fifo.as_ptr(), libc::S_IRUSR | libc::S_IWUSR) };
    assert_eq!(
        result,
        0,
        "create test FIFO: {}",
        std::io::Error::last_os_error()
    );
    let error = inactive_under(&root, &special).err().unwrap();
    assert!(error.to_string().contains("regular file"), "{error}");
}

#[test]
fn a_foreign_prefix_refuses_but_everything_after_first_damage_is_ignored_without_mutation() {
    let dir = OwnDir::for_test("export-first-damage").unwrap();
    let (_, path) = session(&dir);
    let before = dir.read(RECORD).unwrap();
    let mut file = dir.append(RECORD).unwrap();
    file.write_all(b"{}\n").unwrap();
    assert!(
        Snapshot::inactive(
            dir.open_update(RECORD).unwrap(),
            &path,
            crate::record::AgentId::new(0)
        )
        .is_err()
    );
    file.set_len(before.len() as u64).unwrap();
    file.write_all(b"not JSON\n{}\n").unwrap();
    file.write_all(&crate::record::commit_line(&[Display::Answer {
        text: "lost whole commit\n".into(),
    }
    .into()]))
        .unwrap();
    file.write_all(b"\n").unwrap();
    let damaged = dir.read(RECORD).unwrap();
    let text = rendered(&path);
    assert!(!text.contains("lost whole commit"));
    assert_eq!(dir.read(RECORD).unwrap(), damaged);
    assert!(dir.stat("record.jsonl.crash").is_none());
}

#[test]
fn a_live_export_refuses_without_watermark_and_uses_a_bounded_reader_snapshot() {
    let dir = OwnDir::for_test("live-export-watermark").unwrap();
    let (recorder, _) = session(&dir);
    let reader = recorder.reader();
    assert!(reader.snapshot().is_err());
    recorder.sync().unwrap();
    let snapshot = reader.snapshot().unwrap();
    let _later = recorder
        .emit(Display::Answer {
            text: "not in snapshot\n".into(),
        })
        .unwrap();
    let mut out = Vec::new();
    export(&snapshot, &mut out, Format::Plain).unwrap();
    assert!(!String::from_utf8(out).unwrap().contains("not in snapshot"));
    assert!(
        reader.snapshot().is_err(),
        "no separately opened live file may certify the suffix"
    );
}

#[test]
fn export_refuses_mismatched_and_non_tightening_window_records() {
    use crate::record::{AgentId, Control, Selected, Started};

    for (model, context_window) in [("n", 50), ("m", 100), ("m", 101)] {
        let dir = OwnDir::for_test("export-invalid-window").unwrap();
        let path = dir.path().join(RECORD);
        let mut selected = Selected::for_test("m");
        selected.context_window = Some(100);
        let records = [
            Record::Control(Control::SessionStarted(Started {
                session_id: AgentId::new(0),
                name: "main".into(),
                parent: None,
                returns: true,
                selected,
                log_dir: dir.path().to_path_buf(),
                at_unix_ms: 0,
            })),
            Record::Control(Control::WindowNarrowed {
                model: model.into(),
                context_window,
            }),
        ];
        let mut file = std::fs::File::create(&path).unwrap();
        for record in records {
            file.write_all(&crate::record::commit_line(&[record]))
                .unwrap();
            file.write_all(b"\n").unwrap();
        }
        drop(file);
        let error = Snapshot::inactive(dir.open_update(RECORD).unwrap(), &path, AgentId::new(0))
            .err()
            .expect("foreign transition refused before output");
        assert!(
            error.to_string().contains("refused a foreign record"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("the limit must narrow that model"),
            "{error}"
        );
    }
}
