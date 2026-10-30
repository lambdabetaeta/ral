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
    let file = std::fs::File::open(record).expect("the log opens");
    export(file, record, &mut out, Format::Plain).expect("the log exports");
    String::from_utf8(out).expect("the transcript is text")
}

/// A session that asks, reads, is evicted from, is cleared, and asks again.
fn session(dir: &OwnDir) -> (Recorder, std::path::PathBuf) {
    let path = dir.path().join(RECORD);
    let recorder = Recorder::create(dir).expect("a record log");
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
        let error = export(
            std::fs::File::open(&path).unwrap(),
            &path,
            &mut Vec::new(),
            Format::Plain,
        )
        .unwrap_err();
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
