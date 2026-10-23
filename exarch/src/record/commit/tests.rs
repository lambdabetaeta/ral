use super::*;
use crate::bus::{BusReceiver, FleetSink, UsageMeter, channel};
use crate::provider::ProviderError;
use crate::record::AgentId;
use crate::record::Record;
use ral_core::own::{OwnDir, TestDir};

/// The emitter over a log in a directory of its own, which comes back with it
/// and must be held: dropping it deletes the log.
fn emitter() -> (TestDir, Emitter, BusReceiver) {
    let dir = OwnDir::for_test("commit").expect("a test directory");
    let emit = Emitter::create(&dir).expect("temp record log");
    let (tx, rx) = channel();
    emit.attach(Box::new(FleetSink {
        id: AgentId::new(0),
        tx: tx.downgrade(),
        meter: UsageMeter::default(),
    }));
    (dir, emit, rx)
}

fn drain_display(rx: &BusReceiver) -> Vec<Display> {
    crate::bus::drain_records(rx)
        .into_iter()
        .filter_map(|rec| match rec {
            Record::Display(d) => Some(d),
            Record::Protocol(_) | Record::Forensic(_) => None,
        })
        .collect()
}

/// Reasoning precedes the answer inside a step, so its records precede
/// every one of the answer's: the run flushes at the first prose delta,
/// not at the step's end, which would strand it between the lines the
/// chopper had already recorded and the tail it had not.
#[test]
fn a_reasoning_run_records_ahead_of_all_the_prose_that_follows_it() {
    let (_dir, emit, rx) = emitter();
    let mut stream = Stream::default();
    stream.push(&emit, Delta::Think("weighing the cases\n"));
    stream.push(&emit, Delta::Say("First paragraph.\n\n"));
    stream.push(&emit, Delta::Say("Second paragraph.\n\ntail"));
    stream.close(&emit).unwrap();

    let commits = drain_display(&rx);
    match commits.as_slice() {
        [Display::Thinking { text }, rest @ ..] => {
            assert_eq!(text, "weighing the cases\n");
            assert!(
                rest.iter().all(|c| matches!(c, Display::Answer { .. })),
                "nothing but prose follows the run: {rest:?}"
            );
        }
        other => panic!("expected the reasoning run first, got {other:?}"),
    }
}

/// A retry seals the failed attempt's reasoning where it stands: the new
/// attempt's reasoning is a separate record, never a continuation of it.
#[test]
fn a_retry_seals_the_failed_attempts_lanes() {
    let (_dir, emit, rx) = emitter();
    let mut stream = Stream::default();
    let cause = ProviderError::local_transient("model response body read timed out");
    stream.push(&emit, Delta::Think("first try"));
    stream.push(
        &emit,
        Delta::Retry {
            cause: &cause,
            lost: None,
        },
    );
    stream.push(&emit, Delta::Think("second try"));
    stream.push(&emit, Delta::Say("answer"));
    stream.close(&emit).unwrap();

    match drain_display(&rx).as_slice() {
        [
            Display::Thinking { text: first },
            Display::Thinking { text: second },
            Display::Answer { .. },
        ] => {
            assert_eq!(first, "first try");
            assert_eq!(second, "second try");
        }
        other => panic!("expected two thinking records then the answer, got {other:?}"),
    }
}

/// A step that reasons and then calls a tool without a word has no prose
/// seam to flush at, so the boundary flushes it.
#[test]
fn a_wordless_step_seals_its_reasoning_at_the_boundary() {
    let (_dir, emit, rx) = emitter();
    let mut stream = Stream::default();
    stream.push(&emit, Delta::Think("straight to the shell\n"));
    stream.close(&emit).unwrap();

    match drain_display(&rx).as_slice() {
        [Display::Thinking { text }] => assert_eq!(text, "straight to the shell\n"),
        other => panic!("expected the run alone, got {other:?}"),
    }
}

/// The records reassemble the stream exactly: a reader's block is every
/// record of the lane joined, so nothing may be dropped or duplicated
/// between two of them.
#[test]
fn the_records_reassemble_the_whole_stream() {
    let (_dir, emit, rx) = emitter();
    let mut stream = Stream::default();
    let deltas = ["first\n\n", "\n\n", "second\n\n", "tail"];
    for delta in deltas {
        stream.push(&emit, Delta::Say(delta));
    }
    stream.close(&emit).unwrap();

    let recorded: String = drain_display(&rx)
        .into_iter()
        .map(|d| {
            if let Display::Answer { text } = d {
                text
            } else {
                panic!("expected only answer records, got {d:?}")
            }
        })
        .collect();
    assert_eq!(recorded, deltas.concat());
}

/// The cut is the last newline and nothing more: a line records the
/// moment it completes, and the text still short of one waits.  A fence
/// needs no special case, since the block a reader sees joins the records
/// back together.
#[test]
fn a_line_records_where_it_completes_and_the_open_line_waits() {
    let (_dir, emit, rx) = emitter();
    let mut stream = Stream::default();
    stream.push(&emit, Delta::Say("```ral\nlet x = 1"));
    match drain_display(&rx).as_slice() {
        [Display::Answer { text }] => assert_eq!(text, "```ral\n"),
        other => panic!("expected the completed line alone, got {other:?}"),
    }

    stream.push(&emit, Delta::Say("\n```\n"));
    stream.close(&emit).unwrap();
    match drain_display(&rx).as_slice() {
        [Display::Answer { text }] => assert_eq!(text, "let x = 1\n```\n"),
        other => panic!("expected both lines in one record, got {other:?}"),
    }
}

/// Whitespace alone opens no block: it waits for the words after it, so a
/// reader never meets a block holding nothing.
#[test]
fn whitespace_alone_waits_for_the_words_after_it() {
    let (_dir, emit, rx) = emitter();
    let mut stream = Stream::default();
    stream.push(&emit, Delta::Say("\n\n"));
    assert!(
        drain_display(&rx).is_empty(),
        "blank lines alone are not worth a block"
    );

    stream.push(&emit, Delta::Say("a word\n"));
    match drain_display(&rx).as_slice() {
        [Display::Answer { text }] => assert_eq!(text, "\n\na word\n"),
        other => panic!("expected the blank lines to ride along, got {other:?}"),
    }
}
