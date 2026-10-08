use super::*;
use crate::record::model::Held;
use crate::record::{TurnKind, TurnRow};
use genai::chat::{ContentPart, ToolCall};
use regex::Regex;
use std::io::Write as _;

/// A sessions root for one test, deleted when the returned guard falls.
/// Hold the guard: binding only its path deletes the directory on the spot.
fn sessions_root(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("exarch-event-test-{tag}-"))
        .tempdir()
        .expect("sessions root")
}

fn fresh_root() -> AgentLog {
    AgentLog::for_test(
        AgentId::new(0),
        "model",
        &RecordedAccount::for_test("provider"),
    )
    .expect("log")
}

fn assistant_with_tool(id: &str) -> ChatMessage {
    ChatMessage::assistant(vec![ContentPart::ToolCall(ToolCall {
        call_id: id.into(),
        fn_name: "ral".into(),
        fn_arguments: serde_json::json!({"cmd": "pwd"}),
        thought_signatures: None,
    })])
}

fn complete_round(s: &mut AgentLog, user: &str, answer: &str) {
    s.append_user(user.into(), None).unwrap();
    s.append_assistant(ChatMessage::assistant(answer), vec![], None)
        .unwrap();
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a test pattern is a regex")
}

/// The resident turns at or below `through`: what a prefix cut names, now
/// spelled as the set it always was.
fn prefix(log: &AgentLog, through: u64) -> Vec<u64> {
    log.context()
        .context_survey()
        .rows
        .iter()
        .map(|row| row.id)
        .filter(|id| *id <= through)
        .collect()
}

/// Every resident turn of the named prompts: each prompt and the turns
/// answering it, up to the next prompt.
fn answered(log: &AgentLog, prompts: &[u64]) -> Vec<u64> {
    let mut under = false;
    log.context()
        .context_survey()
        .rows
        .iter()
        .filter(|row| {
            if row.role == Role::User {
                under = prompts.contains(&row.id);
            }
            under
        })
        .map(|row| row.id)
        .collect()
}

fn record_path(log: &AgentLog) -> PathBuf {
    log.dir().join("record.jsonl")
}

fn records(log: &AgentLog) -> Vec<Record> {
    crate::record::read_records(&record_path(log)).expect("record.jsonl round-trip")
}
/// Every id-bearing record opens a turn; everything else extends the last.
/// Steering names the turn in hand, so it joins the assistant turn it
/// answers rather than opening one.
#[test]
fn turn_partition_joins_steering_and_opens_one_turn_per_id() {
    let mut s = fresh_root();
    s.record_error("before the first prompt".into()).unwrap();
    s.import_note(ChatMessage::user("inherited")).unwrap();
    complete_round(&mut s, "prompt", "answer");
    s.append_user("tool prompt".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    s.append_tool_results(vec![ToolResult {
        id: "call".into(),
        content: "result".into(),
    }])
    .unwrap();
    s.append_steering("steer".into()).unwrap();
    s.append_assistant(ChatMessage::assistant("finished"), vec![], None)
        .unwrap();

    assert_eq!(
        s.context()
            .transcript_index()
            .iter()
            .map(|row| (row.id, row.role))
            .collect::<Vec<_>>(),
        vec![
            (1, Role::User),
            (2, Role::User),
            (3, Role::Assistant),
            (4, Role::User),
            (5, Role::Assistant),
            (6, Role::Assistant),
        ]
    );
    assert_eq!(s.context().transcript_index()[0].kind, TurnKind::Import);
    assert!(
        !s.context()
            .rendered()
            .iter()
            .any(|message| message.content.first_text() == Some("before the first prompt"))
    );
}

/// One id space, so a prompt's id says nothing about how many came before
/// it — intended.
#[test]
fn ids_mint_monotonically_across_edits() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "one");
    complete_round(&mut s, "two", "two");
    assert_eq!(s.context().current_prompt(), Some(3));
    s.evict(&answered(&s, &[3]), None, EditAuthority::Model)
        .unwrap();
    complete_round(&mut s, "three", "three");
    assert_eq!(s.context().current_prompt(), Some(5));
    s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
        .unwrap();
    complete_round(&mut s, "four", "four");
    assert_eq!(s.context().current_prompt(), Some(7));
    assert_eq!(
        s.context().current_turn(),
        Some(8),
        "the newest turn is the answer"
    );
}

#[test]
fn continues_resolution_requires_the_live_prompt() {
    let mut joins = fresh_root();
    complete_round(&mut joins, "one", "one");
    joins.append_user("nudge".into(), Some(1)).unwrap();
    assert_eq!(joins.context().current_prompt(), Some(1));
    assert_eq!(
        joins.context().transcript_index().len(),
        2,
        "steering opens no turn"
    );

    let mut rewound = fresh_root();
    complete_round(&mut rewound, "one", "one");
    rewound
        .evict(&answered(&rewound, &[1]), None, EditAuthority::Model)
        .unwrap();
    rewound.append_user("fresh".into(), Some(1)).unwrap();
    assert_eq!(rewound.context().current_prompt(), Some(3));

    let mut intervened = fresh_root();
    complete_round(&mut intervened, "one", "one");
    complete_round(&mut intervened, "two", "two");
    intervened.append_user("fresh".into(), Some(1)).unwrap();
    assert_eq!(intervened.context().current_prompt(), Some(5));

    let mut moved_past = fresh_root();
    complete_round(&mut moved_past, "one", "one");
    complete_round(&mut moved_past, "two", "two");
    moved_past
        .evict(&answered(&moved_past, &[3]), None, EditAuthority::Model)
        .unwrap();
    moved_past.append_user("fresh".into(), Some(1)).unwrap();
    assert_eq!(
        moved_past.context().current_prompt(),
        Some(5),
        "turn 1 is no longer the live prompt, whatever is still in context"
    );
}

/// The three refusals an eviction makes by name, and the empty set.
#[test]
fn evict_refuses_the_unclosed_turn_the_unknown_the_departed_and_the_empty_set() {
    let mut live = fresh_root();
    live.append_user("live".into(), None).unwrap();
    assert_eq!(
        live.evict(&[1], None, EditAuthority::Model).unwrap_err(),
        "turn 1 is being written now: an eviction keeps the work in hand"
    );

    let mut unknown = fresh_root();
    complete_round(&mut unknown, "one", "one");
    assert_eq!(
        unknown.evict(&[7], None, EditAuthority::Model).unwrap_err(),
        "turn 7 is not recorded: the latest is 2"
    );
    assert_eq!(
        unknown.evict(&[], None, EditAuthority::Model).unwrap_err(),
        "an eviction must name at least one turn"
    );

    complete_round(&mut unknown, "two", "two");
    unknown
        .evict(&prefix(&unknown, 2), None, EditAuthority::Harness)
        .unwrap();
    assert_eq!(
        unknown.evict(&[1], None, EditAuthority::Model).unwrap_err(),
        "turn 1 has already left your context: the earliest still in it is 3"
    );
}

/// A prompt one of whose answers the set does not name stays with it; a
/// set the rule would empty is refused, and says which turns to add.
#[test]
fn the_survivor_rule_keeps_a_prompt_and_refuses_a_cut_that_takes_nothing() {
    let mut s = fresh_root();
    s.append_user("work on the parser".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    s.append_tool_results(vec![ToolResult {
        id: "call".into(),
        content: "result".into(),
    }])
    .unwrap();
    s.append_assistant(ChatMessage::assistant("done"), vec![], None)
        .unwrap();
    assert_eq!(
        s.evict(&[1], None, EditAuthority::Model).unwrap_err(),
        "turn 1 is the prompt whose turns 2–3 are still in your context, and a prompt \
         stays with them: name them too, or leave it"
    );
    s.evict(&[1, 2], None, EditAuthority::Model).unwrap();
    assert_eq!(
        s.context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![1, 3],
        "the prompt stays with the reply that survived the cut"
    );
}

/// One [`TranscriptTurn`] per turn named, in transcript order, each
/// addressed by its own `turn` field rather than by argument order. A
/// turn marker carries no model content, so it contributes no message.
#[test]
fn the_door_answers_one_element_per_turn() {
    let mut s = fresh_root();
    s.append_user("first prompt".into(), None).unwrap();
    s.record_turn_start(Tuning::default(), None).unwrap();
    s.append_assistant(ChatMessage::assistant("first answer"), vec![], None)
        .unwrap();
    complete_round(&mut s, "second prompt", "second answer");

    let read = s
        .context()
        .read_transcript(&[1, 2])
        .expect("closed turns are readable");
    let [prompt, reply] = read.as_slice() else {
        panic!("two named turns answer two records, got {read:?}")
    };
    assert_eq!((prompt.turn, prompt.role), (1, Role::User));
    assert_eq!((reply.turn, reply.role), (2, Role::Assistant));
    let [user_msg] = prompt.messages.as_slice() else {
        panic!(
            "a turn marker carries no message, got {:?}",
            prompt.messages
        )
    };
    assert_eq!(user_msg.role, ChatRole::User);
    assert!(
        matches!(user_msg.parts.as_slice(), [TranscriptPart::Text(text)] if text == "first prompt")
    );
    let [assistant_msg] = reply.messages.as_slice() else {
        panic!("one turn, one message, got {:?}", reply.messages)
    };
    assert_eq!(assistant_msg.role, ChatRole::Assistant);
    assert!(
        matches!(assistant_msg.parts.as_slice(), [TranscriptPart::Text(text)] if text == "first answer")
    );

    complete_round(&mut s, "third prompt", "third answer");
    assert_eq!(
        s.rewind(9).unwrap_err(),
        "turn 9 is not recorded: the latest is 6"
    );
}

/// A rewind leaves the structure as it stood before the anchor opened:
/// no row, no hole, the anchor's id minted again by the next prompt, and
/// the whole of it rebuilt by replay.
#[test]
fn a_rewind_removes_the_anchor_on_and_the_next_prompt_takes_its_id() {
    let sessions = sessions_root("rewind");
    let mut s = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut s, "one", "one");
    complete_round(&mut s, "two", "two");
    complete_round(&mut s, "three", "three");
    s.evict(&answered(&s, &[1]), None, EditAuthority::Model)
        .unwrap();

    s.rewind(3).expect("a recorded anchor");
    assert_eq!(
        s.context()
            .transcript_index()
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the index reaches nothing from the anchor on"
    );
    assert_eq!(
        s.context().rendered().len(),
        1,
        "the hole's marker stands alone"
    );
    assert!(s.context().is_ready());
    assert_eq!(s.context().next_id(), 3);

    complete_round(&mut s, "three again", "again");
    assert_eq!(s.context().current_turn(), Some(4));
    s.rewind(1).expect("a departed anchor may be rewound to");
    assert_eq!(s.context().rendered().len(), 0, "the context may empty");
    assert_eq!(s.context().next_id(), 1);
    complete_round(&mut s, "fresh", "fresh");

    let expected = serde_json::to_vec(&s.context().rendered().iter().collect::<Vec<_>>()).unwrap();
    drop(s);
    let resumed = AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume after rewinds");
    assert_eq!(
        serde_json::to_vec(&resumed.context().rendered().iter().collect::<Vec<_>>()).unwrap(),
        expected
    );
    assert_eq!(resumed.context().current_turn(), Some(2));
}

/// A rewind is refused while a batch is in flight; the anchor named must
/// exist.
#[test]
fn a_rewind_waits_for_the_batch_in_flight() {
    let mut s = fresh_root();
    s.append_user("first".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    assert_eq!(
        s.rewind(1).unwrap_err(),
        "cannot rewind while the session is waiting for 1 tool result"
    );
}

/// The marker standing at the head hole — message 0 — or `None` when the
/// context does not open with a hole.
fn head_marker(s: &AgentLog) -> Option<String> {
    s.context()
        .rendered()
        .first()
        .and_then(|message| message.content.first_text())
        .filter(|text| text.contains("left your context"))
        .map(str::to_string)
}

/// A cut leaves a prompt with a surviving answer standing, and the marker
/// says which turns went.
#[test]
fn a_cut_between_a_prompt_and_its_answer_keeps_the_prompt() {
    let mut s = fresh_root();
    s.append_user("work on the parser".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    s.append_tool_results(vec![ToolResult {
        id: "call".into(),
        content: "result".into(),
    }])
    .unwrap();
    s.append_assistant(ChatMessage::assistant("done"), vec![], None)
        .unwrap();
    assert_eq!(
        s.context()
            .transcript_index()
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    s.evict(&prefix(&s, 2), None, EditAuthority::Model).unwrap();
    assert_eq!(
        s.context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![1, 3],
        "the prompt stays with the reply that survived the cut"
    );
    // The marker stands where the turn did: after the prompt, before the
    // reply that survived it.
    let rendered: Vec<String> = s
        .context()
        .rendered()
        .iter()
        .map(|message| message.content.first_text().unwrap_or_default().to_string())
        .collect();
    let [prompt, marker, done] = rendered.as_slice() else {
        panic!("the prompt, one marker, and the surviving reply, got {rendered:?}")
    };
    assert_eq!(prompt, "work on the parser");
    assert!(
        marker.starts_with("[EXARCH // Turn 2 has left your context."),
        "{marker}"
    );
    assert!(
        marker.contains(
            "`exarch-transcript `read [turns: !{range 2 3}]` reads turn 2 back as material"
        ),
        "{marker}"
    );
    assert_eq!(done, "done");
}

#[test]
fn evict_of_evict_indexes_both_cuts_and_keeps_the_suffix() {
    let mut s = fresh_root();
    for prompt in ["one", "two", "three"] {
        complete_round(&mut s, prompt, prompt);
    }
    s.evict(
        &prefix(&s, 2),
        Some("the parser is fixed".into()),
        EditAuthority::Model,
    )
    .unwrap();
    s.evict(&prefix(&s, 4), None, EditAuthority::Harness)
        .unwrap();

    assert_eq!(s.context().notes().len(), 2);
    assert_eq!(
        s.context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![5, 6]
    );
    let marker = head_marker(&s).expect("two cuts render one marker");
    assert!(
        marker.starts_with("[EXARCH // Turns 1–4 have left your context."),
        "{marker}"
    );
    assert!(marker.contains("Your note: \"the parser is fixed\""));
}

/// The prompt cache reads message 0 byte for byte, so the marker must
/// depend on the table and its cuts and nothing else — not on how many
/// times it is rendered, and not on whether the fold was built live or
/// refolded.
#[test]
fn head_marker_is_a_pure_function_of_the_table() {
    let sessions = sessions_root("head-marker-purity");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    for prompt in ["one", "two", "three"] {
        complete_round(&mut live, prompt, prompt);
    }
    live.evict(
        &prefix(&live, 4),
        Some("keep going".into()),
        EditAuthority::Model,
    )
    .unwrap();
    let first = head_marker(&live).expect("a cut renders a marker");
    assert_eq!(head_marker(&live).as_ref(), Some(&first));
    drop(live);

    let resumed = AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume");
    assert_eq!(head_marker(&resumed), Some(first));
}

#[test]
fn head_marker_collapses_rows_past_the_cap() {
    let mut s = fresh_root();
    for n in 1..=45u64 {
        let prompt = format!("prompt {n}");
        complete_round(&mut s, &prompt, "answer");
    }
    // 45 rounds hold turns 1..90; a cut through turn 88 takes 88 of them.
    s.evict(&prefix(&s, 88), None, EditAuthority::Harness)
        .unwrap();
    let marker = head_marker(&s).expect("a cut renders a marker");
    assert!(
        marker.contains("1–48  (48 earlier turns: exarch-transcript `index)"),
        "the rows past the cap collapse to one line, got: {marker}"
    );
    assert!(
        !marker.lines().any(|line| line.starts_with("   1  "))
            && marker.lines().any(|line| line.starts_with("  49  ")),
        "the collapsed rows are the oldest, got: {marker}"
    );
    assert_eq!(
        marker.lines().count(),
        1 + 1 + 40,
        "one sentence, one collapse line, and the capped rows"
    );
}

/// A note is drawn only alongside a fragment of its own cut, so a cut
/// collapsed away entirely takes its note with it — message 0 stays
/// bounded by the row cap rather than growing with how many cuts the
/// session has run.
#[test]
fn a_note_collapses_with_its_rows() {
    let mut s = fresh_root();
    for n in 1..=45u64 {
        let prompt = format!("prompt {n}");
        complete_round(&mut s, &prompt, "answer");
        if n > 1 {
            s.evict(
                &prefix(&s, 2 * (n - 1)),
                Some(format!("note {n}")),
                EditAuthority::Model,
            )
            .unwrap();
        }
    }
    let marker = head_marker(&s).expect("44 cuts render one marker");
    assert!(
        marker.contains("\"note 45\""),
        "the newest note must survive, got: {marker}"
    );
    assert!(
        !marker.contains("\"note 2\""),
        "a note collapsed with its rows must not survive, got: {marker}"
    );
    assert_eq!(
        marker.lines().count(),
        1 + 1 + 40 + 20,
        "bounded by the row cap: 40 rows, and a note per cut that kept one"
    );
}

/// A later cut opens a hole of its own, so the provider re-reads from
/// there and message 0 stays byte for byte what it was. A cut adjacent
/// to the head hole joins it instead: a hole is a maximal run.
#[test]
fn a_late_cut_leaves_the_head_marker_untouched() {
    let mut s = fresh_root();
    for prompt in ["one", "two", "three", "four", "five"] {
        complete_round(&mut s, prompt, prompt);
    }
    s.evict(&prefix(&s, 4), None, EditAuthority::Harness)
        .unwrap();
    let before = head_marker(&s).expect("a cut renders a marker");
    s.evict(&answered(&s, &[7]), None, EditAuthority::Model)
        .unwrap();
    assert_eq!(head_marker(&s), Some(before));
    assert!(
        s.context()
            .transcript_index()
            .iter()
            .filter(|row| [7, 8].contains(&row.id))
            .all(|row| row.held == Held::Evicted { cut: 1 })
    );
    assert_eq!(
        s.context().rendered().len(),
        6,
        "marker, turns 5 and 6, marker, turns 9 and 10"
    );
}

#[test]
fn evict_refuses_a_turn_already_gone_and_the_unclosed_one() {
    let mut s = fresh_root();
    for prompt in ["one", "two", "three"] {
        complete_round(&mut s, prompt, prompt);
    }
    s.evict(&prefix(&s, 4), None, EditAuthority::Harness)
        .unwrap();
    assert_eq!(
        s.evict(&[2], None, EditAuthority::Model).unwrap_err(),
        "turn 2 has already left your context: the earliest still in it is 5"
    );
    s.append_user("four".into(), None).unwrap();
    assert_eq!(
        s.evict(&[7], None, EditAuthority::Model).unwrap_err(),
        "turn 7 is being written now: an eviction keeps the work in hand"
    );
    assert_eq!(
        s.evict(&[99], None, EditAuthority::Model).unwrap_err(),
        "turn 99 is not recorded: the latest is 7"
    );
}

/// What comes back from the log is what the model was sent: the records
/// hold the clipped strings, and one rendering serves both.
#[test]
fn read_transcript_reads_evicted_turns_byte_identically() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "answer one");
    complete_round(&mut s, "two", "answer two");
    let before = format!(
        "{:?}",
        s.context().read_transcript(&[1, 2]).expect("in context")
    );
    s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
        .unwrap();
    let after = format!(
        "{:?}",
        s.context()
            .read_transcript(&[1, 2])
            .expect("evicted but recorded")
    );
    assert_eq!(after, before);
    assert_eq!(
        s.context().read_transcript(&[9]).unwrap_err(),
        "turn 9 is not recorded: the latest is 4"
    );
}

/// A locus measures a file, not a path: bytes that changed under a live
/// range are refused as the mismatch they are, not as bad JSON.
#[test]
fn a_record_that_does_not_hash_to_its_locus_is_refused_by_name() {
    let mut s = fresh_root();
    complete_round(&mut s, "alpha", "alpha answered");
    complete_round(&mut s, "beta", "beta answered");
    s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
        .unwrap();

    // Equal-length substitution: every locus's byte range still names a
    // whole, well-formed record — just not the one it measured.
    let path = record_path(&s);
    let recorded = fs::read_to_string(&path).expect("record.jsonl is utf-8");
    let rewritten = recorded.replace("alpha", "gamma");
    assert_ne!(recorded, rewritten, "the substitution must reach the log");
    assert_eq!(
        recorded.len(),
        rewritten.len(),
        "the loci's byte ranges must still hold whole records"
    );
    fs::write(&path, &rewritten).expect("rewrite record.jsonl in place");

    let refusal = s
        .context()
        .read_transcript(&[1, 2])
        .expect_err("a record that no longer hashes to its locus is unreadable");
    assert!(
        refusal.contains("did not hash to the locus that named it"),
        "the refusal must name the digest mismatch, got: {refusal}"
    );
}

/// The index is every turn the transcript holds, whatever took it out of
/// the context — and the turn in flight is listed like any other.
#[test]
fn transcript_index_lists_every_turn_with_how_it_is_held() {
    let mut s = fresh_root();
    for prompt in ["one", "two", "three"] {
        complete_round(&mut s, prompt, prompt);
    }
    s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
        .unwrap();
    s.evict(&answered(&s, &[3]), None, EditAuthority::Model)
        .unwrap();
    s.append_user("live".into(), None).unwrap();

    let index = s.context().transcript_index();
    assert_eq!(
        index
            .iter()
            .map(|row| (row.id, row.role, row.held))
            .collect::<Vec<_>>(),
        vec![
            (1, Role::User, Held::Evicted { cut: 0 }),
            (2, Role::Assistant, Held::Evicted { cut: 0 }),
            (3, Role::User, Held::Evicted { cut: 1 }),
            (4, Role::Assistant, Held::Evicted { cut: 1 }),
            (5, Role::User, Held::Resident),
            (6, Role::Assistant, Held::Resident),
            (7, Role::User, Held::Resident),
        ]
    );
    assert_eq!(index[0].label, "one");
    assert!(
        index
            .iter()
            .all(|row| row.bytes > 0 && row.kind == TurnKind::Own)
    );
}

/// The one turn no door reads back is the one being written; the closed
/// turns beside it read back like any other.
#[test]
fn the_door_refuses_the_turn_being_written() {
    let mut s = fresh_root();
    s.append_user("work".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    s.append_tool_results(vec![ToolResult {
        id: "call".into(),
        content: "result".into(),
    }])
    .unwrap();
    assert_eq!(
        s.context().read_transcript(&[1, 2]).unwrap_err(),
        "turn 2 is being written now: it is the one turn the transcript cannot read back yet"
    );
    assert_eq!(
        s.context()
            .read_transcript(&[1])
            .expect("the closed prompt reads back")
            .len(),
        1
    );
    assert_eq!(
        s.context().read_transcript(&[]).unwrap_err(),
        "`turns` names no turn: `!{range a b}` builds a run of ids"
    );
}

/// One search over both sides of the ledger: the freed records read back
/// off `record.jsonl`, the resident ones rendered in place, and the hits
/// in transcript order across the seam between them.
#[test]
fn grep_walks_resident_then_freed() {
    let mut s = fresh_root();
    complete_round(&mut s, "the parser is broken", "I will fix the parser");
    complete_round(&mut s, "the lexer is fine", "nothing to do");
    s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
        .unwrap();

    let answer = s
        .context()
        .grep_transcript(&regex(r"\bthe\b"), None)
        .expect("the whole transcript is searchable");
    assert_eq!(answer.total, 3);
    assert_eq!(
        answer
            .hits
            .iter()
            .map(|hit| (hit.turn, hit.role.clone(), hit.line))
            .collect::<Vec<_>>(),
        vec![
            (1, ChatRole::User, 1),
            (2, ChatRole::Assistant, 1),
            (3, ChatRole::User, 1),
        ]
    );
    assert_eq!(answer.hits[0].text, "the parser is broken");

    let narrowed = s
        .context()
        .grep_transcript(&regex(r"\bthe\b"), Some(&[1, 4]))
        .expect("a list with a gap searches exactly what it names");
    assert_eq!(narrowed.total, 1, "turns 2 and 3 are not searched");
    assert_eq!(narrowed.hits[0].turn, 1);
    assert_eq!(
        s.context()
            .grep_transcript(&regex("parser"), Some(&[9]))
            .unwrap_err(),
        "turn 9 is not recorded: the latest is 4"
    );
    assert_eq!(
        s.context()
            .grep_transcript(&regex("parser"), Some(&[]))
            .unwrap_err(),
        "`turns` names no turn: omit it to search the whole transcript"
    );
}

/// A pattern that matches everything answers one page, not the whole
/// transcript: `total` is what tells the model to narrow.
#[test]
fn grep_caps_hits_and_reports_total() {
    let mut s = fresh_root();
    let many = (1..=150)
        .map(|n| format!("line {n} matches"))
        .collect::<Vec<_>>()
        .join("\n");
    complete_round(&mut s, "count", &many);

    let answer = s
        .context()
        .grep_transcript(&regex("matches"), None)
        .expect("the whole transcript is searchable");
    assert_eq!(answer.total, 150);
    assert_eq!(answer.hits.len(), 100);
    assert_eq!(answer.hits[0].line, 1);
    assert_eq!(
        answer.hits[99].line, 100,
        "the oldest hundred, in transcript order"
    );
}

#[test]
fn eviction_plan_walks_back_from_the_turn_in_hand() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "one");
    complete_round(&mut s, "two", "two");
    complete_round(&mut s, "three", "three");
    let rows = s.context().context_survey().rows;
    let keep = rows[4].bytes + rows[5].bytes;
    let plan = s.context().plan_eviction(keep).expect("old turns to shed");
    assert_eq!(plan, vec![1, 2, 3, 4]);
    s.evict(&plan, None, EditAuthority::Harness).unwrap();
    assert_eq!(
        s.context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![5, 6],
        "the planned turns left the context"
    );
    assert!(
        !s.context()
            .rendered()
            .iter()
            .any(|message| { message.content.first_text() == Some("one") })
    );
}

/// A turn in hand heavy enough to fill the whole keep budget on its own
/// must never be the one a plan names: that would leave the model with
/// nothing, and the set would be one `resolve_cut` refuses.
#[test]
fn a_plan_never_takes_the_newest_turn() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "one");
    let big = "x".repeat(100_000);
    complete_round(&mut s, "two", &big);
    let keep = s.context().history_bytes() / 2;
    let plan = s
        .context()
        .plan_eviction(keep)
        .expect("the older turns to shed");
    assert_eq!(plan, vec![1, 2]);
    s.evict(&plan, None, EditAuthority::Harness).unwrap();
    assert_eq!(
        s.context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![3, 4],
        "the work in hand must survive the eviction it could not itself be named by"
    );
}

#[test]
fn a_lone_prompt_is_never_planned_away() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "one");
    assert!(s.context().plan_eviction(0).is_none());
}

#[test]
fn a_user_ending_import_turn_is_never_torn() {
    let mut s = fresh_root();
    s.import_note(ChatMessage::user("imported user")).unwrap();
    complete_round(&mut s, "normal", "answer");

    let rendered = s.context().rendered();
    assert_eq!(
        rendered
            .iter()
            .filter_map(|message| message.content.first_text())
            .collect::<Vec<_>>(),
        vec!["imported user", "normal", "answer"]
    );
}

/// Work abandoned before any reply is closed by the next prompt,
/// not by a fabricated one: nothing is recorded in its place, and the
/// model reads it exactly as it lies, the next prompt following.
#[test]
fn abandoned_work_is_kept_whole_and_read_whole() {
    let mut s = fresh_root();
    s.append_user("interrupted".into(), None).unwrap();
    s.quiesce(QuiesceReason::Cancelled);
    assert!(s.context().is_ready());
    assert!(
        !records(&s)
            .iter()
            .any(|record| matches!(record, Record::Protocol(Protocol::AssistantMessage { .. }))),
        "no assistant turn the model never took may enter the log"
    );
    complete_round(&mut s, "next", "answer");
    let rendered = s.context().rendered();
    let read: Vec<&str> = rendered
        .iter()
        .filter_map(|message| message.content.first_text())
        .collect();
    assert_eq!(read, vec!["interrupted", "next", "answer"]);
}

/// The work an interrupted turn did stays in the context: an agentic
/// run is one prompt and then many tool turns, and an Esc that dropped
/// them all would leave the model to rediscover its own session.
#[test]
fn abandoned_work_keeps_its_tool_turns_in_context() {
    let mut s = fresh_root();
    s.append_user("run the tool".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    s.append_tool_results(vec![ToolResult {
        id: "call".into(),
        content: "result".into(),
    }])
    .unwrap();
    s.quiesce(QuiesceReason::Cancelled);
    complete_round(&mut s, "next", "answer");
    assert_eq!(
        s.context()
            .rendered()
            .iter()
            .map(|message| message.role.clone())
            .collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::User,
            ChatRole::Assistant
        ]
    );
}

/// Tool calls that never ran are the one thing a quiesce still owes: the
/// calls were really made, and a dangling tool-call block is not a legal
/// request.
#[test]
fn quiesce_answers_tool_calls_that_never_ran_and_nothing_else() {
    let mut s = fresh_root();
    s.append_user("run the tool".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    assert!(
        !s.context().is_ready(),
        "outstanding tool calls hold the log"
    );
    s.quiesce(QuiesceReason::Aborted);
    assert!(s.context().is_ready());
    assert!(records(&s).iter().any(|record| matches!(
        record,
        Record::Protocol(Protocol::ToolResults { results })
            if results.iter().any(|result| result.id == "call")
    )));
    assert!(
        !records(&s).iter().any(|record| matches!(
            record,
            Record::Protocol(Protocol::AssistantMessage { stop_reason: Some(r), .. })
                if r == "aborted"
        )),
        "the answer is owed; a closing assistant turn is not"
    );
    s.append_user("next".into(), None).unwrap();
}

/// A `reply` ends the work it was asked for, so its capstone is earned —
/// and without one the fold could not tell it from an interruption, and
/// would read the whole of that work as its note.
#[test]
fn a_reply_keeps_its_capstone_and_stays_visible() {
    let mut s = fresh_root();
    s.append_user("do the work".into(), None).unwrap();
    s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    s.append_tool_results(vec![ToolResult {
        id: "call".into(),
        content: "done".into(),
    }])
    .unwrap();
    s.quiesce(QuiesceReason::Replied);
    complete_round(&mut s, "follow-up", "answer");
    assert!(
        s.context().rendered().iter().any(|message| {
            message.content.first_text()
                == Some("[EXARCH // Deliberation ended: replied to parent.]")
        }),
        "a replied turn stays whole in the child's own context"
    );
}

/// The recorded set is the resolved set: what the writer worked out is
/// what the file carries, so replay departs exactly these ids.
#[test]
fn record_jsonl_round_trip_carries_the_resolved_cut() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "one");
    complete_round(&mut s, "two", "two");
    s.evict(
        &[1, 2, 3],
        Some("the parser is fixed".into()),
        EditAuthority::Model,
    )
    .unwrap();

    assert!(
        records(&s).iter().any(|record| {
            matches!(
                record,
                Record::Protocol(Protocol::Evicted {
                    cut,
                    by: EditAuthority::Model,
                }) if cut.turns == [1, 2] && cut.note.as_deref() == Some("the parser is fixed")
            )
        }),
        "turn 3 is the prompt turn 4 still answers, and never reaches the file"
    );
}

/// A record naming a turn no live session could have evicted is the
/// hand-edited file it is.
#[test]
fn resume_refuses_an_eviction_of_a_turn_not_in_the_context() {
    let sessions = sessions_root("resume-foreign-eviction");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut live, "one", "one");
    let path = sessions.path().join("0/record.jsonl");
    drop(live);
    let foreign = Record::Protocol(Protocol::Evicted {
        cut: Cut {
            turns: vec![9],
            note: None,
        },
        by: EditAuthority::Model,
    });
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&crate::record::envelope_line(&foreign))
        .unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();

    let error = AgentLog::resume(sessions.path(), AgentId::new(0))
        .err()
        .expect("an eviction of a turn not in the context is foreign data");
    let text = error.to_string();
    assert!(
        text.contains("evicts turn 9, which is not in the context"),
        "{text}"
    );
}

#[test]
fn diagnostic_records_round_trip_through_record_jsonl() {
    let mut s = fresh_root();
    s.record_error("boom".into()).unwrap();
    s.record_nudge("stop=length".into(), Some(Spent { used: 2, max: 3 }))
        .unwrap();
    let parsed = records(&s);
    assert!(parsed.iter().any(
        |record| matches!(record, Record::Forensic(Forensic::Error { text }) if text == "boom")
    ));
    assert!(parsed.iter().any(|record| matches!(
        record,
        Record::Forensic(Forensic::Nudge { cause, spent: Some(Spent { used: 2, max: 3 }) }) if cause == "stop=length"
    )));
}

/// A child's opening bookend names the selection it was handed, not the
/// one its parent's file opened with: a spawn may send the child to
/// another model, and a `/model` since has already moved the parent's own.
#[test]
fn a_forked_log_records_the_selection_it_was_handed() {
    let parent = fresh_root();
    let child = parent
        .fork(
            AgentId::new(1),
            0,
            &RecordedModel::for_test("other-model"),
            &RecordedAccount::for_test("other-provider"),
        )
        .expect("child log");
    let opened = records(&child)
        .into_iter()
        .find_map(|r| match r {
            Record::Forensic(Forensic::SessionStarted { model, label, .. }) => Some((model, label)),
            _ => None,
        })
        .expect("the child's opening bookend");
    assert_eq!(
        opened,
        ("other-model".to_string(), "other-provider".to_string())
    );
}

#[test]
fn clear_rotates_record_jsonl_and_resets_the_table() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "one");
    complete_round(&mut s, "two", "two");
    s.evict(&answered(&s, &[3]), None, EditAuthority::Model)
        .unwrap();

    let record = s.dir().join("record.jsonl");
    s.clear(0, 2).expect("clear");
    assert!(record.with_extension("jsonl.0").exists());
    assert_eq!(s.context().transcript_index(), Vec::<TurnRow>::new());
    assert!(s.context().is_ready());
}

#[test]
fn a_departed_turn_keeps_no_resident_state() {
    let mut s = fresh_root();
    complete_round(&mut s, "one", "one");
    s.evict(&answered(&s, &[1]), None, EditAuthority::Model)
        .unwrap();
    assert_eq!(
        s.context().event_count(),
        1,
        "no turn's records are owned any more; the hole's marker is what stands"
    );
}

#[test]
fn resume_replays_a_scripted_history_and_preserves_the_context() {
    let sessions = sessions_root("resume-round-trip");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    live.import_note(ChatMessage::user("inherited")).unwrap();
    complete_round(&mut live, "one", "answer one");
    complete_round(&mut live, "two", "answer two");
    live.evict(&prefix(&live, 3), None, EditAuthority::Harness)
        .unwrap();
    let expected =
        serde_json::to_vec(&live.context().rendered().iter().collect::<Vec<_>>()).unwrap();
    drop(live);

    let resumed = AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume");
    assert!(resumed.context().is_ready());
    assert_eq!(
        serde_json::to_vec(&resumed.context().rendered().iter().collect::<Vec<_>>()).unwrap(),
        expected
    );
}

#[test]
fn resume_quiesces_a_torn_turn_after_reopening_append_mode() {
    let sessions = sessions_root("resume-mid-turn");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    live.append_user("run the tool".into(), None).unwrap();
    live.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    drop(live);

    let mut resumed = AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume");
    assert!(resumed.context().is_ready());
    let recorded = records(&resumed);
    assert!(
        recorded
            .iter()
            .any(|record| matches!(record, Record::Protocol(Protocol::ToolResults { .. })))
    );
    resumed.append_user("continue".into(), None).unwrap();
    let recorded = records(&resumed);
    assert!(matches!(
        recorded.last(),
        Some(Record::Protocol(Protocol::UserPrompt { text, .. })) if text == "continue"
    ));
}

#[test]
fn resume_quarantines_only_an_unterminated_final_fragment() {
    let sessions = sessions_root("resume-tail");
    let live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    let path = sessions.path().join("0/record.jsonl");
    drop(live);
    let prefix = fs::read(&path).unwrap();
    let fragment = b"{\"Protocol\":{\"kind\":\"user_prompt\"";
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(fragment).unwrap();
    file.flush().unwrap();

    let resumed =
        AgentLog::resume(sessions.path(), AgentId::new(0)).expect("torn tail is recoverable");
    assert!(resumed.context().is_ready());
    assert_eq!(fs::read(&path).unwrap(), prefix);
    assert_eq!(
        fs::read(sessions.path().join("0/record.jsonl.crash")).unwrap(),
        fragment
    );
}

#[test]
fn resume_refuses_a_complete_garbage_line_without_mutating_the_file() {
    let sessions = sessions_root("resume-garbage");
    let live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    let path = sessions.path().join("0/record.jsonl");
    drop(live);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"garbage\n").unwrap();
    file.flush().unwrap();
    let before = fs::read(&path).unwrap();

    assert!(
        AgentLog::resume(sessions.path(), AgentId::new(0)).is_err(),
        "garbage is not a crash tail"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn resume_refuses_foreign_protocol_data_without_mutating_the_file() {
    let sessions = sessions_root("resume-foreign");
    let live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    let path = sessions.path().join("0/record.jsonl");
    drop(live);
    let foreign = Record::Protocol(Protocol::AssistantMessage {
        turn: 1,
        message: ChatMessage::user("wrong role"),
        pending_tool_ids: Vec::new(),
        stop_reason: None,
    });
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&crate::record::envelope_line(&foreign))
        .unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();
    let before = fs::read(&path).unwrap();

    let error = AgentLog::resume(sessions.path(), AgentId::new(0))
        .err()
        .expect("foreign data must refuse");
    let text = error.to_string();
    assert!(text.contains("foreign protocol data"), "{text}");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn resume_refuses_a_steering_line_with_no_turn_to_steer() {
    let sessions = sessions_root("resume-steering-no-turn");
    let live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    let path = sessions.path().join("0/record.jsonl");
    drop(live);
    let orphan = Record::Protocol(Protocol::Steering {
        text: "steer".into(),
    });
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&crate::record::envelope_line(&orphan))
        .unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();

    let error = AgentLog::resume(sessions.path(), AgentId::new(0))
        .err()
        .expect("a steering line with no turn is foreign data");
    let text = error.to_string();
    assert!(
        text.contains("steers a turn, but no turn has been recorded"),
        "{text}"
    );
}

#[test]
fn resume_refuses_a_stale_id_as_foreign_data() {
    let sessions = sessions_root("resume-stale-id");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut live, "one", "one");
    complete_round(&mut live, "two", "two");
    let path = sessions.path().join("0/record.jsonl");
    drop(live);
    let stale = Record::Protocol(Protocol::UserPrompt {
        turn: 1,
        text: "stale".into(),
    });
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&crate::record::envelope_line(&stale))
        .unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();
    let before = fs::read(&path).unwrap();

    let error = AgentLog::resume(sessions.path(), AgentId::new(0))
        .err()
        .expect("a stale id is foreign data");
    let text = error.to_string();
    assert!(text.contains("names turn 1"), "{text}");
    assert!(text.contains("moved past"), "{text}");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn resume_refuses_nonzero_session_ids_as_transient_children() {
    let sessions = sessions_root("resume-child");
    let child = AgentLog::root(
        sessions.path(),
        AgentId::new(1),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    drop(child);
    let error = AgentLog::resume(sessions.path(), AgentId::new(1))
        .err()
        .expect("child logs are transient");
    assert!(
        error
            .to_string()
            .contains("children are transient by design")
    );
}

#[test]
fn resume_refuses_a_pre_plan_session_with_no_record_log() {
    let sessions = sessions_root("resume-pre-plan");
    let dir = sessions.path().join("0");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("events.jsonl"), b"").unwrap();

    let error = AgentLog::resume(sessions.path(), AgentId::new(0))
        .err()
        .expect("a directory with no record.jsonl must refuse, not start empty");
    let text = error.to_string();
    assert!(text.contains("record.jsonl"), "{text}");
    assert!(!dir.join("record.jsonl").exists());
}

/// A live lineage quiesces its unready tail before it records anything
/// else, so an interior gap is never something it wrote: the fold refuses
/// it as the hand-edited file it is, rather than stubbing it shut.
#[test]
fn resume_refuses_an_interior_tool_answer_lost_from_disk() {
    let sessions = sessions_root("resume-interior-seam");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    live.append_user("first".into(), None).unwrap();
    live.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    live.quiesce(QuiesceReason::Aborted);
    complete_round(&mut live, "second", "answer");
    let path = sessions.path().join("0/record.jsonl");
    drop(live);

    let data = fs::read(&path).unwrap();
    let mut torn = Vec::with_capacity(data.len());
    for fragment in data.split_inclusive(|byte| *byte == b'\n') {
        let record = crate::record::record_from_line(&fragment[..fragment.len() - 1]).unwrap();
        if matches!(record, Record::Protocol(Protocol::ToolResults { .. })) {
            continue;
        }
        torn.extend_from_slice(fragment);
    }
    fs::write(&path, &torn).unwrap();

    let error = AgentLog::resume(sessions.path(), AgentId::new(0))
        .err()
        .expect("an interior gap is a hand-edited file");
    let text = error.to_string();
    assert!(text.contains("foreign protocol data"), "{text}");
    assert!(
        text.contains("not a seam that quiesce can repair"),
        "{text}"
    );
    assert_eq!(fs::read(&path).unwrap(), torn);
}

#[test]
fn resume_matches_live_after_a_small_edit_sequence_family() {
    for pattern in 0..9 {
        let sessions = sessions_root(&format!("resume-edits-{pattern}"));
        let mut live = AgentLog::root(
            sessions.path(),
            AgentId::new(0),
            &RecordedModel::for_test("model"),
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut live, "one", "one");
        complete_round(&mut live, "two", "two");
        complete_round(&mut live, "three", "three");
        match pattern {
            0 => {}
            1 => {
                live.evict(&answered(&live, &[3]), None, EditAuthority::Model)
                    .unwrap();
            }
            2 => {
                live.evict(&prefix(&live, 4), None, EditAuthority::Harness)
                    .unwrap();
            }
            3 => {
                live.evict(&answered(&live, &[1]), None, EditAuthority::Model)
                    .unwrap();
                live.evict(&answered(&live, &[3]), None, EditAuthority::Model)
                    .unwrap();
            }
            4 => {
                live.evict(
                    &prefix(&live, 4),
                    Some("halfway".into()),
                    EditAuthority::Harness,
                )
                .unwrap();
                live.evict(&answered(&live, &[5]), None, EditAuthority::Model)
                    .unwrap();
            }
            5 => {
                live.evict(&answered(&live, &[3]), None, EditAuthority::Model)
                    .unwrap();
                complete_round(&mut live, "four", "four");
            }
            6 => {
                live.evict(&prefix(&live, 2), None, EditAuthority::Harness)
                    .unwrap();
                live.evict(
                    &prefix(&live, 4),
                    Some("second cut".into()),
                    EditAuthority::Model,
                )
                .unwrap();
            }
            7 => {
                live.record_error("forensics".into()).unwrap();
                live.record_nudge("retry".into(), Some(Spent { used: 1, max: 2 }))
                    .unwrap();
            }
            8 => {
                live.evict(&prefix(&live, 2), None, EditAuthority::Harness)
                    .unwrap();
                live.rewind(3).unwrap();
                complete_round(&mut live, "three again", "again");
            }
            _ => unreachable!(),
        }
        let expected =
            serde_json::to_vec(&live.context().rendered().iter().collect::<Vec<_>>()).unwrap();
        drop(live);
        let resumed =
            AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume edit sequence");
        assert!(resumed.context().is_ready());
        assert_eq!(
            serde_json::to_vec(&resumed.context().rendered().iter().collect::<Vec<_>>()).unwrap(),
            expected,
            "pattern {pattern}"
        );
    }
}

/// The structure is a fold of the log, so replaying the file rebuilds
/// every row and every note the live session held — weights included, a
/// misplaced departed record showing up as a marker that had drifted.
#[test]
fn resume_rebuilds_the_same_rows_and_notes() {
    let sessions = sessions_root("fold-equals-memo-evict");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    live.append_user("interrupted".into(), None).unwrap();
    live.quiesce(QuiesceReason::Cancelled);
    for prompt in ["one", "two", "three"] {
        complete_round(&mut live, prompt, prompt);
    }
    live.evict(
        &prefix(&live, 5),
        Some("the abandoned work is indexed too".into()),
        EditAuthority::Model,
    )
    .unwrap();
    let rows = live.context().transcript_index();
    let notes = live.context().notes().to_vec();
    drop(live);

    let resumed = AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume");
    assert_eq!(resumed.context().transcript_index(), rows);
    assert_eq!(resumed.context().notes(), notes.as_slice());
}

/// A departed turn's address is not written down anywhere: the fold mints
/// it from the loci the records arrived with, so a resumed session reads
/// the same bytes back with nothing carried over but the file.
#[test]
fn a_departed_turn_reads_back_after_resume() {
    let sessions = sessions_root("resume-departed-read");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut live, "one", "answer one");
    complete_round(&mut live, "two", "answer two");
    live.evict(&prefix(&live, 2), None, EditAuthority::Harness)
        .unwrap();
    let before = format!(
        "{:?}",
        live.context().read_transcript(&[1, 2]).expect("evicted")
    );
    drop(live);

    let resumed = AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume");
    assert_eq!(
        resumed
            .context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![3, 4],
        "the cut crossed the resume"
    );
    let after = format!(
        "{:?}",
        resumed
            .context()
            .read_transcript(&[1, 2])
            .expect("the pointer was rebuilt by the fold alone")
    );
    assert_eq!(after, before);
}

/// A `mnemon` child of `parent`: its own log under the same sessions
/// root, opened with the table and the link the parent hands over.
fn mnemon(parent: &AgentLog, child_id: AgentId) -> AgentLog {
    let inherited = parent.inherited_context();
    let mut child = parent
        .fork(
            child_id,
            0,
            &RecordedModel::for_test("model"),
            &RecordedAccount::for_test("provider"),
        )
        .expect("child log");
    child.import_context(inherited).expect("import");
    child
}

fn seed_has_a_tool_call(log: &AgentLog) -> bool {
    log.context().rendered().iter().any(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolCall(_)))
    })
}

/// An `Evicted` record with no id of its own glues onto whatever turn is
/// still open, so an eviction run mid-batch lands right after the
/// dangling assistant frame rather than replacing it. The seed must still
/// cut at the tool call, not at the eviction that followed it.
#[test]
fn a_seed_cut_never_owes_a_tool_result() {
    let mut parent = fresh_root();
    complete_round(&mut parent, "one", "one");
    parent.append_user("two".into(), None).unwrap();
    parent
        .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
        .unwrap();
    parent
        .evict(&answered(&parent, &[1]), None, EditAuthority::Model)
        .unwrap();

    let child = mnemon(&parent, AgentId::new(1));
    assert!(
        !seed_has_a_tool_call(&child),
        "a mnemon seed must never carry an unanswered tool call"
    );
    assert!(
        child
            .context()
            .rendered()
            .iter()
            .any(|message| message.content.first_text() == Some("two")),
        "the prompt behind the dangling call must still seed"
    );
}

/// The plain in-batch fork: no edit lands after the dangling assistant
/// frame, so the frame itself is the turn's last record, and the cut
/// must still land in front of it.
#[test]
fn a_seed_cut_never_owes_a_tool_result_with_no_edit() {
    let mut parent = fresh_root();
    complete_round(&mut parent, "one", "one");
    parent.append_user("two".into(), None).unwrap();
    parent
        .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
        .unwrap();

    let child = mnemon(&parent, AgentId::new(1));
    assert!(
        !seed_has_a_tool_call(&child),
        "the plain in-batch fork must not regress"
    );
    assert!(
        child
            .context()
            .rendered()
            .iter()
            .any(|message| message.content.first_text() == Some("two")),
    );
}

/// A turn's transcript copy is where it was first recorded. The seed cuts
/// in front of the dangling tool call, so the child's own copy of that
/// turn is short — and the door still answers the call, off the parent's
/// file, before and after the child evicts the turn.
#[test]
fn a_seeded_turn_reads_back_from_its_origin() {
    let sessions = sessions_root("lineage-origin");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut parent, "one", "one");
    parent.append_user("two".into(), None).unwrap();
    parent
        .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
        .unwrap();

    let mut child = mnemon(&parent, AgentId::new(1));
    assert!(
        !seed_has_a_tool_call(&child),
        "the child's own copy of the turn stops in front of the call"
    );
    let read = child
        .context()
        .read_transcript(&[3, 4])
        .expect("the turn is recorded in the parent's file");
    let called = |read: &[TranscriptTurn]| {
        read.iter()
            .flat_map(|turn| &turn.messages)
            .flat_map(|message| &message.parts)
            .any(|part| matches!(part, TranscriptPart::Program { source, .. } if source == "pwd"))
    };
    assert!(
        called(&read),
        "the origin answers the whole turn, not the seed's short copy"
    );

    complete_round(&mut child, "the child's own", "answer");
    child
        .evict(&prefix(&child, 4), None, EditAuthority::Harness)
        .unwrap();
    let departed = child
        .context()
        .read_transcript(&[3, 4])
        .expect("eviction points at the copy, never at the seed");
    assert_eq!(format!("{departed:?}"), format!("{read:?}"));
    assert!(called(&departed));
}

fn openings(read: &[TranscriptTurn]) -> Vec<&str> {
    read.iter()
        .flat_map(|turn| &turn.messages)
        .filter_map(|message| match message.parts.first() {
            Some(TranscriptPart::Text(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The lineage is one address space: what the parent evicted before the
/// fork is still the child's to read, under the number the parent gave
/// it, and the child's own ids start above every one of them.
#[test]
fn a_mnemon_child_reads_an_ancestors_evicted_turns_and_mints_ids_above_it() {
    let sessions = sessions_root("lineage-read");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    for prompt in ["one", "two", "three", "four"] {
        complete_round(&mut parent, prompt, prompt);
    }
    parent
        .evict(&prefix(&parent, 6), None, EditAuthority::Harness)
        .unwrap();

    let mut child = mnemon(&parent, AgentId::new(1));
    assert_eq!(
        child
            .context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![7, 8],
        "the parent's surviving turns cross under the parent's own ids"
    );
    complete_round(&mut child, "the child's own", "answer");
    assert_eq!(child.context().current_prompt(), Some(9));

    let read = child
        .context()
        .read_transcript(&[3, 4])
        .expect("an ancestor's evicted turns are in the lineage's transcript");
    assert_eq!(
        read.iter().map(|turn| turn.turn).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(
        read.iter().map(|turn| turn.role).collect::<Vec<_>>(),
        vec![Role::User, Role::Assistant]
    );
    assert_eq!(openings(&read), vec!["two", "two"]);

    let mut grandchild = mnemon(&child, AgentId::new(2));
    let read = grandchild
        .context()
        .read_transcript(&[3, 4])
        .expect("two links back is still the same transcript");
    assert_eq!(openings(&read), vec!["two", "two"]);
    complete_round(&mut grandchild, "the grandchild's own", "answer");
    assert_eq!(grandchild.context().current_prompt(), Some(11));
}

/// A cut between a prompt and its answers, then a fork, leaves them in
/// two files: the evicted turn in the ancestor's, the survivors
/// re-recorded as the child's own. Each turn reads back from wherever its
/// placement says, so the read answers them all rather than the local half
/// of them.
#[test]
fn turns_split_across_a_fork_read_back_out_of_both_files() {
    let sessions = sessions_root("lineage-split");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    parent
        .append_user("work on the parser".into(), None)
        .unwrap();
    parent
        .append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
        .unwrap();
    parent
        .append_tool_results(vec![ToolResult {
            id: "call".into(),
            content: "result".into(),
        }])
        .unwrap();
    parent
        .append_assistant(ChatMessage::assistant("done"), vec![], None)
        .unwrap();
    parent
        .evict(&prefix(&parent, 2), None, EditAuthority::Harness)
        .unwrap();

    let child = mnemon(&parent, AgentId::new(1));
    let read = child
        .context()
        .read_transcript(&[1, 2, 3])
        .expect("both halves are in the lineage's transcript");
    assert_eq!(
        read.iter().map(|turn| turn.turn).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        read.iter()
            .flat_map(|turn| &turn.messages)
            .map(|message| message.role.clone())
            .collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Assistant
        ],
        "the evicted turn's records come off the ancestor's file, the survivors off the child's"
    );
}

/// A `/rewind` at a ready boundary can leave a parent with no turn in
/// context at all; the ids it minted are still spent, and the child says
/// so.
#[test]
fn an_empty_parent_context_still_floors_the_childs_first_id() {
    let sessions = sessions_root("lineage-floor");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut parent, "one", "one");
    complete_round(&mut parent, "two", "two");
    parent
        .evict(&answered(&parent, &[1, 3]), None, EditAuthority::Model)
        .unwrap();
    assert_eq!(parent.context().context_survey().rows.len(), 0);

    let mut child = mnemon(&parent, AgentId::new(1));
    assert_eq!(child.context().context_survey().rows.len(), 0);
    child.append_user("first".into(), None).unwrap();
    assert_eq!(child.context().current_prompt(), Some(5));
}

/// Only a fork's opening may link a log to an ancestry: a link found
/// after the log has a context of its own is foreign data.
#[test]
fn admission_refuses_inherited_after_the_first_turn() {
    let sessions = sessions_root("lineage-late-link");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut live, "one", "one");
    let path = sessions.path().join("0/record.jsonl");
    drop(live);
    let late = Record::Protocol(Protocol::Inherited {
        turns: Vec::new(),
        notes: Vec::new(),
    });
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&crate::record::envelope_line(&late))
        .unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();

    let error = AgentLog::resume(sessions.path(), AgentId::new(0))
        .err()
        .expect("a late link is foreign data");
    let text = error.to_string();
    assert!(text.contains("only a fork's opening may do"), "{text}");
}

/// A pointer whose file will not answer refuses the turn behind it by
/// path and by the fault the read actually met — a line that will not
/// read back, or a file that will not open — and never by calling an
/// turn the lineage recorded unrecorded. Nothing remembers the
/// break, so the second read tries the file again.
#[test]
fn a_broken_ancestry_link_is_refused_by_path_and_fault() {
    let sessions = sessions_root("lineage-broken");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    for prompt in ["one", "two"] {
        complete_round(&mut parent, prompt, prompt);
    }
    parent
        .evict(&prefix(&parent, 2), None, EditAuthority::Harness)
        .unwrap();
    let source = record_path(&parent);
    let child = mnemon(&parent, AgentId::new(1));
    drop(parent);

    let recorded = fs::read_to_string(&source).expect("record.jsonl is utf-8");
    let mut torn = String::new();
    for (line, text) in recorded.lines().enumerate() {
        if line == 1 {
            torn.push_str("{\"not\":\"an envelope\"}\n");
        }
        torn.push_str(text);
        torn.push('\n');
    }
    fs::write(&source, &torn).expect("rewrite the ancestor's log");
    let refusal = child
        .context()
        .read_transcript(&[1, 2])
        .expect_err("a line the read cannot follow stops the lineage");
    assert!(
        refusal.starts_with(&format!(
            "turn 1 could not be read back from {}: ",
            source.display()
        )) && refusal.contains("did not hash to the locus that named it"),
        "{refusal}"
    );

    fs::remove_file(&source).expect("delete the ancestor's log");
    let refusal = child
        .context()
        .read_transcript(&[1, 2])
        .expect_err("a deleted ancestor's log stops the lineage");
    assert!(
        refusal.starts_with(&format!(
            "turn 1 could not be read back from {}: ",
            source.display()
        )),
        "{refusal}"
    );
}

/// The link is folded, not just recorded: replaying it rebuilds the
/// parent's rows and its notes, so a resume covers an inherited context
/// as it covers an evicted one — and the marker at the child's head hole,
/// being that same projection, carries the parent's own note.
#[test]
fn resume_rebuilds_the_same_rows_and_notes_across_a_link() {
    let ancestry = sessions_root("fold-equals-memo-ancestor");
    let mut ancestor = AgentLog::root(
        ancestry.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    for prompt in ["one", "two", "three"] {
        complete_round(&mut ancestor, prompt, prompt);
    }
    ancestor
        .evict(
            &prefix(&ancestor, 4),
            Some("the parser is fixed".into()),
            EditAuthority::Model,
        )
        .unwrap();
    let inherited = ancestor.inherited_context();

    let sessions = sessions_root("fold-equals-memo-inherited");
    let mut live = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    live.import_context(inherited).unwrap();
    assert_eq!(
        live.context().notes(),
        [Some("the parser is fixed".to_string())],
        "the parent's cut crosses the link"
    );
    let head = live
        .context()
        .rendered()
        .first()
        .expect("an inherited context that was cut renders a head marker")
        .content
        .first_text()
        .unwrap_or_default()
        .to_string();
    assert!(
        head.starts_with("[EXARCH //") && head.contains("the parser is fixed"),
        "the child's marker is the same projection of the same fold: {head}"
    );

    complete_round(&mut live, "own", "own");
    live.evict(&prefix(&live, 6), None, EditAuthority::Harness)
        .unwrap();
    let rows = live.context().transcript_index();
    let notes = live.context().notes().to_vec();
    drop(live);

    let resumed = AgentLog::resume(sessions.path(), AgentId::new(0)).expect("resume");
    assert_eq!(resumed.context().transcript_index(), rows);
    assert_eq!(resumed.context().notes(), notes.as_slice());
}

/// The index is the whole lineage's table, in id order: a child inherits
/// every turn its ancestry recorded, `held` as the parent had it, and its
/// own turns land above them.
#[test]
fn transcript_index_carries_the_inherited_table() {
    let sessions = sessions_root("lineage-index");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    for prompt in ["one", "two", "three"] {
        complete_round(&mut parent, prompt, prompt);
    }
    parent
        .evict(&prefix(&parent, 4), None, EditAuthority::Harness)
        .unwrap();

    let mut child = mnemon(&parent, AgentId::new(1));
    complete_round(&mut child, "four", "four");

    let index = child.context().transcript_index();
    assert_eq!(
        index
            .iter()
            .map(|row| (row.id, row.kind, row.held))
            .collect::<Vec<_>>(),
        vec![
            (1, TurnKind::Inherited, Held::Evicted { cut: 0 }),
            (2, TurnKind::Inherited, Held::Evicted { cut: 0 }),
            (3, TurnKind::Inherited, Held::Evicted { cut: 0 }),
            (4, TurnKind::Inherited, Held::Evicted { cut: 0 }),
            (5, TurnKind::Inherited, Held::Resident),
            (6, TurnKind::Inherited, Held::Resident),
            (7, TurnKind::Own, Held::Resident),
            (8, TurnKind::Own, Held::Resident),
        ]
    );
    assert_eq!(index[0].label, "one");
}

/// One search over the whole lineage: the child's resident turns, its own
/// freed records, and then the ancestor's file — in transcript order
/// across all three, and never the same turn twice.
#[test]
fn grep_walks_resident_then_freed_then_ancestors() {
    let sessions = sessions_root("lineage-grep");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut parent, "the parser is broken", "I will fix the parser");
    complete_round(&mut parent, "the lexer is fine", "nothing to do");
    parent
        .evict(&prefix(&parent, 2), None, EditAuthority::Harness)
        .unwrap();

    let mut child = mnemon(&parent, AgentId::new(1));
    complete_round(&mut child, "the tests pass", "good");
    child
        .evict(&prefix(&child, 4), None, EditAuthority::Harness)
        .unwrap();

    let answer = child
        .context()
        .grep_transcript(&regex(r"\bthe\b"), None)
        .expect("the lineage's whole transcript is searchable");
    assert_eq!(answer.total, 4);
    assert_eq!(
        answer
            .hits
            .iter()
            .map(|hit| (hit.turn, hit.role.clone()))
            .collect::<Vec<_>>(),
        vec![
            (1, ChatRole::User),
            (2, ChatRole::Assistant),
            (3, ChatRole::User),
            (5, ChatRole::User),
        ]
    );
    assert_eq!(answer.hits[0].text, "the parser is broken");
}

/// A search of the whole transcript answers what it can reach and passes
/// over what it cannot — the marker at its hole has already told the model
/// which turns it cannot have back. A narrowing that names one of them is
/// refused instead, by the path that would not answer.
#[test]
fn an_unnarrowed_grep_passes_over_an_unreadable_file() {
    let sessions = sessions_root("lineage-unreadable");
    let mut parent = AgentLog::root(
        sessions.path(),
        AgentId::new(0),
        &RecordedModel::for_test("model"),
        &RecordedAccount::for_test("provider"),
        0,
    )
    .unwrap();
    complete_round(&mut parent, "the parser is broken", "I will fix the parser");
    let source = record_path(&parent);
    let mut child = mnemon(&parent, AgentId::new(1));
    drop(parent);
    complete_round(&mut child, "the tests pass", "good");
    fs::remove_file(&source).expect("delete the ancestor's log");

    let answer = child
        .context()
        .grep_transcript(&regex(r"\bthe\b"), None)
        .expect("an unnarrowed search answers what it can reach");
    assert_eq!(answer.total, 1);
    assert_eq!(answer.hits[0].text, "the tests pass");

    let refusal = child
        .context()
        .read_transcript(&[1, 2])
        .expect_err("a read that names an unreadable turn is refused");
    assert!(
        refusal.starts_with(&format!(
            "turn 1 could not be read back from {}: ",
            source.display()
        )),
        "{refusal}"
    );
}
