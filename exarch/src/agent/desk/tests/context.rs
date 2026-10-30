//! The context and transcript families.

use super::*;
use crate::record::Span;

fn context_evict_request(from: u64, to: u64, note: &str) -> Request {
    Request::Context(Context::Evict(Evict {
        span: Span::new(from, to).expect("a test names a run first turn first"),
        note: Note(note.to_string()),
    }))
}

/// An empty address is well-typed, so a read naming no turn is the shape
/// the fold refuses rather than one the encoder cannot build.
fn transcript_read_request(turns: &[u64]) -> Request {
    Request::Transcript(Transcript::Read(Reading {
        turns: turns.to_vec(),
    }))
}

fn transcript_grep_request(pattern: &str, turns: Option<&[u64]>) -> Request {
    Request::Transcript(Transcript::Grep(Grep {
        pattern: Regex::new(pattern).expect("a test pattern compiles"),
        turns: turns.map_or(Turns::All, |t| Turns::Only(t.to_vec())),
    }))
}

/// One row per frontier item by its span: an eviction where the run was, then
/// the turns no eviction covers.
#[test]
fn context_survey_rows_every_item_in_the_context() {
    with_desk(|desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "closed\nturn", "answer");
            append_prompt_and_answer(log, "evicted", "answer");
            log.evict(
                Span::new(1, 2).expect("a run"),
                Some("closed".into()),
                EditAuthority::Model,
            )
            .expect("evict");
            log.import_note(genai::chat::ChatMessage::user("inherited\ncontext"))
                .expect("import");
            log.append_user("live".into(), None, Voice::Human)
                .expect("live prompt");
        }
        let expected_bytes = desk.loan.get().unwrap().log.context().history_bytes();

        let answer = desk
            .ask(Request::Context(Context::Survey))
            .expect("context survey");
        let rows = survey_rows(&answer)
            .iter()
            .map(|row| {
                (
                    int_field(row, "from"),
                    int_field(row, "to"),
                    str_field(row, "kind").expect("survey kind"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                (1, 2, "eviction"),
                (3, 3, "own"),
                (4, 4, "own"),
                (5, 5, "import"),
                (6, 6, "own")
            ]
        );
        assert_eq!(
            int_field(&answer, "total-bytes"),
            i64::try_from(expected_bytes).unwrap()
        );
    });
}

/// One record per turn the read named, not one concatenated blob: the
/// list is the shape the doc's own "read in slices" advice needs to be
/// sayable.  A read is a listing, so it commits no act and draws no row.
#[test]
fn transcript_answers_one_record_per_turn_without_committing_an_act() {
    with_desk(|mut desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "first prompt", "first answer");
        }
        let (tx, rx) = channel();
        desk.loan
            .get()
            .unwrap()
            .log
            .record_emitter()
            .attach(Box::new(FleetSink {
                id: AgentId::new(0),
                tx: tx.downgrade(),
                meter: crate::bus::UsageMeter::default(),
            }));
        desk.call.emit = Emitter::new(tx, AgentId::new(0));

        let FOValue::List { items } = desk
            .ask(transcript_read_request(&[1, 2]))
            .expect("exarch-transcript `read")
        else {
            panic!("exarch-transcript `read must answer a list, one record per turn")
        };
        let [first, second] = items.as_slice() else {
            panic!("two named turns must answer exactly two records, got {items:?}")
        };
        assert_eq!(int_field(first, "turn"), 1);
        assert_eq!(str_field(first, "role"), Some("user"));
        let messages = first
            .field("messages")
            .and_then(FOValue::as_list)
            .unwrap_or_else(|| panic!("a read's messages are a list, got {first:?}"));
        assert_eq!(
            messages.len(),
            1,
            "the prompt turn holds one message, got {messages:?}"
        );
        assert_eq!(
            int_field(second, "turn"),
            2,
            "the second record is the turn asked for after it"
        );
        assert!(desk.call.audit().is_none(), "a read has no act");

        desk.ask(transcript_read_request(&[]))
            .expect_err("a read that names no turn is not meaningful");
        let drawn = crate::bus::drain_records(&rx)
            .into_iter()
            .filter(|record| matches!(record, Record::Display(Display::HarnessCall { .. })))
            .count();
        assert_eq!(
            drawn, 0,
            "neither the read nor its refusal draws an act row: a listing's telling is its answer"
        );
    });
}

/// A read answers the turns it named wherever they lie, each naming the
/// role it bears; a turn past what is recorded, and the turn
/// being written, are refused by the state they meet.
#[test]
fn transcript_read_names_the_role_of_every_turn_it_answers() {
    with_desk(|desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "first prompt", "first answer");
            append_prompt_and_answer(log, "second prompt", "second answer");
        }
        let FOValue::List { items } = desk
            .ask(transcript_read_request(&[2, 3]))
            .expect("closed turns are readable")
        else {
            panic!("exarch-transcript `read must answer a list, one record per turn")
        };
        let reached = items
            .iter()
            .map(|item| {
                (
                    int_field(item, "turn"),
                    str_field(item, "role").expect("a read names the turn's role"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reached,
            vec![(2, "assistant"), (3, "user")],
            "an answer and the prompt after it, each naming its own role"
        );

        let error = desk
            .ask(transcript_read_request(&[9]))
            .expect_err("a read must not reach past what is recorded");
        assert_eq!(error.message, "turn 9 is not recorded: the latest is 4");

        desk.loan
            .get()
            .unwrap()
            .log
            .append_user("live".into(), None, Voice::Human)
            .expect("live prompt");
        let error = desk
            .ask(transcript_read_request(&[5]))
            .expect_err("the turn being written is not readable");
        assert_eq!(
            error.message,
            "turn 5 is being written now: it is the one turn the transcript cannot read back yet"
        );
    });
}

/// The pattern is compiled on decode, so an invalid one is refused in the
/// regex crate's own words rather than in a paraphrase of them.
#[test]
#[expect(
    clippy::invalid_regex,
    reason = "the pattern that will not compile is this test's subject"
)]
fn grep_refuses_a_bad_regex_with_the_crate_message() {
    const UNCLOSED: &str = "(unclosed";
    with_desk(|desk, _agent| {
        let expected = Regex::new(UNCLOSED)
            .expect_err("an unclosed group is not a regex")
            .to_string();
        let error = desk
            .handle(&family_req(
                "transcript",
                "grep",
                Some(FOValue::Map {
                    entries: vec![("pattern".to_string(), text(UNCLOSED))],
                }),
            ))
            .expect_err("an invalid regex is not searchable");
        assert!(error.message.ends_with(&expected), "got: {}", error.message);
    });
}

/// `turn` for `turns` once searched the whole transcript in silence: the
/// reader accepted the payload and no one ever asked for that field.
#[test]
fn transcript_grep_refuses_a_field_it_does_not_read() {
    with_desk(|desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "first prompt", "first answer");
        }
        let err = desk
            .handle(&family_req(
                "transcript",
                "grep",
                Some(FOValue::Map {
                    entries: vec![
                        ("pattern".to_string(), text("answer")),
                        (
                            "turn".to_string(),
                            FOValue::List {
                                items: vec![FOValue::Int { value: 1 }],
                            },
                        ),
                    ],
                }),
            ))
            .expect_err("`turn` is not a field `grep` reads");
        assert_eq!(
            err.message,
            "`` exarch-transcript `grep ``: unknown field `turn`: did you mean `turns`?"
        );
        desk.ask(transcript_grep_request("answer", Some(&[1, 2])))
            .expect("the spelt field still narrows the search");
    });
}

/// The edit's answer is the survey the transition leaves behind, not a
/// receipt for the transition: the number that decides the next edit is
/// `total-bytes` now, against the budget. The act it commits is
/// [`DeskAct::ContextEvict`], and the audit sentence names it.
#[test]
fn context_evict_answers_the_survey_it_leaves_behind_and_commits_the_act() {
    with_desk(|desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "one", "a longer answer");
            append_prompt_and_answer(log, "two", "another answer");
        }
        let answer = desk
            .ask(context_evict_request(1, 2, "the parser is fixed"))
            .expect("context evict");
        assert_eq!(
            int_field(&answer, "total-bytes"),
            i64::try_from(desk.loan.get().unwrap().log.context().history_bytes()).unwrap(),
            "the survey's total is the context's own weight"
        );
        let rows = survey_rows(&answer);
        assert_eq!(
            rows.iter()
                .map(|row| (
                    int_field(row, "from"),
                    int_field(row, "to"),
                    str_field(row, "role").expect("a survey row names its role")
                ))
                .collect::<Vec<_>>(),
            vec![(1, 2, "user"), (3, 3, "user"), (4, 4, "assistant")],
            "one eviction stands where the run was"
        );
        assert_eq!(str_field(&rows[0], "kind"), Some("eviction"));
        assert_eq!(str_field(&rows[0], "label"), Some("the parser is fixed"));
        assert_eq!(str_field(&rows[1], "kind"), Some("own"));
        let audit = desk.call.audit().expect("a landed eviction leaves an act");
        assert!(
            audit.contains("evicted context"),
            "the audit sentence must name the act, got: {audit}"
        );
    });
}

/// The survey rows an answer carries, in id order.
fn survey_rows(answer: &FOValue) -> &[FOValue] {
    answer
        .field("rows")
        .and_then(FOValue::as_list)
        .expect("a context answer carries its survey rows")
}

/// The eviction keeps a note as one short line for the rest of the session,
/// so an empty one, an overlong one, and one that breaks a line are refused
/// at decode rather than drawn.
#[test]
fn context_evict_refuses_a_note_that_is_not_one_short_line() {
    with_desk(|desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "one", "answer");
            append_prompt_and_answer(log, "two", "answer");
        }
        let single_line = "`note: text must be a single line: the eviction keeps one short line";
        let cases = [
            (
                "",
                "`note: text must not be empty: the eviction keeps one short line, and it is the \
             only trace of the run left in your context",
            ),
            (
                &*"x".repeat(241),
                "`note: text is 241 bytes; the eviction keeps one short line: 240 at most. What is \
             the one thing your future self needs to know?",
            ),
            ("line one\nline two", single_line),
            ("line one\rline two", single_line),
        ];
        for (note, refusal) in cases {
            let err = desk
                .ask(context_evict_request(1, 1, note))
                .expect_err("a note that is not one short line is refused");
            assert_eq!(
                err.message,
                format!("`` exarch-context `evict ``: {refusal}")
            );
        }
    });
}

/// A field no tag reads is refused, not dropped: an eviction that silently
/// ignored `through` would cut turns the model never named.
#[test]
fn context_evict_refuses_a_field_it_does_not_read() {
    with_desk(|desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "one", "answer");
            append_prompt_and_answer(log, "two", "answer");
        }
        let err = desk
            .handle(&family_req(
                "context",
                "evict",
                Some(FOValue::Map {
                    entries: vec![
                        ("from".to_string(), FOValue::Int { value: 1 }),
                        ("through".to_string(), FOValue::Int { value: 2 }),
                    ],
                }),
            ))
            .expect_err("`through` is not a field `evict` reads");
        assert_eq!(
            err.message,
            "`` exarch-context `evict ``: unknown field `through`: expected `from`, `to`, `note`"
        );
        desk.ask(context_evict_request(1, 2, "the parser is fixed"))
            .expect("the fields the tag does read still evict");
    });
}

/// The index rows every item the lineage recorded by its span, an eviction
/// before what it covers, each naming what holds it; a grep hit on a note
/// names the eviction's whole span.
#[test]
fn the_index_and_grep_name_items_by_span() {
    with_desk(|desk, _agent| {
        {
            let mut body = desk.loan.get().unwrap();
            let log = &mut body.log;
            append_prompt_and_answer(log, "one", "answer");
            append_prompt_and_answer(log, "two", "answer");
        }
        desk.ask(context_evict_request(3, 4, "the parser is fixed"))
            .expect("context evict");
        let index = desk
            .ask(Request::Transcript(Transcript::Index))
            .expect("exarch-transcript `index");
        let index = index.as_list().expect("the index is a list");
        let spans = index
            .iter()
            .map(|row| {
                (
                    int_field(row, "from"),
                    int_field(row, "to"),
                    str_field(row, "kind").expect("an index row names its kind"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            vec![
                (1, 1, "own"),
                (2, 2, "own"),
                (3, 4, "eviction"),
                (3, 3, "own"),
                (4, 4, "own")
            ]
        );
        let int = |value| FOValue::Int { value };
        assert_eq!(index[2].field("location"), Some(&tag("resident", None)));
        assert_eq!(
            index[3].field("location"),
            Some(&tag(
                "evicted",
                Some(FOValue::Map {
                    entries: vec![("from".into(), int(3)), ("to".into(), int(4))]
                })
            ))
        );

        let hits = desk
            .ask(transcript_grep_request("parser", None))
            .expect("exarch-transcript `grep");
        let hits = hits
            .field("hits")
            .and_then(FOValue::as_list)
            .expect("a grep answers hits");
        let [hit] = hits else {
            panic!("the note is the one line that matches, got {hits:?}")
        };
        assert_eq!(
            (
                int_field(hit, "from"),
                int_field(hit, "to"),
                str_field(hit, "kind"),
                int_field(hit, "line")
            ),
            (3, 4, Some("eviction"), 1)
        );
    });
}
