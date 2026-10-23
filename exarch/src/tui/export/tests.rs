use super::*;
use crate::record::RECORD;
use crate::record::{
    BlockId, Display, EditAuthority, Emitter as Recorder, Protocol, Residue, Span, Verdict, Voice,
};
use ral_core::fact::Read;
use ral_core::first_order::datum::Datum as _;
use ral_core::own::OwnDir;
use ral_core::types::{Observation, Observed};

fn prompt(turn: u64, text: &str) -> Protocol {
    Protocol::UserPrompt {
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
        Protocol::Evicted {
            residue: Residue {
                span: Span::single(1),
                note: None,
                by: EditAuthority::Harness,
            },
        }
        .into(),
    );
    let _ = emit(Protocol::Cleared.into());
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
    file.write_all(br#"{"at_unix_ms":0,"record":{"kind":"user_pro"#)
        .expect("a torn write");
    assert_eq!(rendered(&path), whole);
}
