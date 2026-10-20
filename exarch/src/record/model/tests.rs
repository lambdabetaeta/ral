use super::render::render_residue;
use super::table::holder;
use super::*;
use crate::record::{
    AgentId, AgentLog, GrepHit, Inherited, RecordedAccount, Seq, ToolResult, Voice,
};
use regex::Regex;

/// A turn in the context. Its records are elided: every fold under test
/// here reads `bytes`, never the material.
fn here(id: u64, role: Role, label: &str, bytes: usize) -> Turn {
    Turn {
        id,
        role,
        kind: TurnKind::Own,
        label: label.to_string(),
        bytes,
        body: Body::Here {
            records: Vec::new(),
            origin: None,
        },
    }
}

/// A turn under the residue over `holder`.
fn there(id: u64, holder: Span, role: Role, label: &str, bytes: usize) -> Turn {
    Turn {
        body: Body::There {
            at: Pointer {
                source: PathBuf::from("record.jsonl"),
                loci: Vec::new(),
            },
            departure: Departure::Under(holder),
        },
        ..here(id, role, label, bytes)
    }
}

fn context(turns: Vec<Turn>, residues: Vec<Residue>) -> Context {
    Context {
        table: Table::seeded(PathBuf::from("record.jsonl"), turns, residues),
        ..Context::new(PathBuf::from("record.jsonl"))
    }
}

/// The model writes a note; a forced eviction has none.
fn by(note: Option<&str>) -> EditAuthority {
    if note.is_some() {
        EditAuthority::Model
    } else {
        EditAuthority::Harness
    }
}

fn span(from: u64, to: u64) -> Span {
    Span::new(from, to).expect("a test names a run first turn first")
}

fn residue(from: u64, to: u64, note: Option<&str>) -> Residue {
    Residue {
        span: span(from, to),
        note: note.map(str::to_string),
        by: by(note),
    }
}

fn under(from: u64, to: u64) -> Location {
    Location::Under(span(from, to))
}

/// One record through the live fold. The table is what these tests read,
/// so a placeholder locus is address enough.
fn step(context: &mut Context, protocol: Protocol) -> Result<(), Refusal> {
    let seq = Seq::new(context.log_len() as u64 + 1);
    context.step(Recorded::new(Locus::placeholder(seq), protocol))
}

/// Evict through the fold, as a recorded cut does.
fn evict(context: &mut Context, from: u64, to: u64, note: Option<&str>) -> Result<(), Refusal> {
    step(
        context,
        Protocol::Evicted {
            residue: residue(from, to, note),
        },
    )
}

fn prompt(context: &mut Context, text: &str) {
    let turn = context.next_id();
    step(
        context,
        Protocol::UserPrompt {
            turn,
            text: text.into(),
            voice: Voice::Human,
        },
    )
    .expect("a prompt at rest");
}

fn reply(context: &mut Context, text: &str) {
    let turn = context.next_id();
    step(
        context,
        Protocol::AssistantMessage {
            turn,
            message: ChatMessage::assistant(text),
            pending_tool_ids: Vec::new(),
            stop_reason: None,
        },
    )
    .expect("a reply where one is awaited");
}

/// An assistant turn that calls a tool, and the result answering it.
fn call(context: &mut Context, result: &str) {
    let turn = context.next_id();
    step(
        context,
        Protocol::AssistantMessage {
            turn,
            message: called(None),
            pending_tool_ids: vec!["t1".into()],
            stop_reason: None,
        },
    )
    .expect("a call where a reply is awaited");
    step(
        context,
        Protocol::ToolResults {
            results: vec![ToolResult {
                id: "t1".into(),
                content: result.into(),
            }],
        },
    )
    .expect("the result the call awaits");
}

/// `n` closed exchanges through the live fold: leaves `1..=2n`, prompts odd.
fn exchanges(n: u64) -> Context {
    let mut context = Context::new(PathBuf::from("record.jsonl"));
    for k in 1..=n {
        prompt(&mut context, &format!("prompt {k}"));
        reply(&mut context, &format!("reply {k}"));
    }
    context
}

/// A log on disk, for what reads records back through their pointers.
fn log() -> AgentLog {
    AgentLog::for_test(
        AgentId::new(0),
        "model",
        &RecordedAccount::for_test("provider"),
    )
    .expect("log")
}

fn cut(log: &mut AgentLog, from: u64, to: u64, note: Option<&str>) {
    log.evict(span(from, to), note.map(str::to_string), by(note))
        .expect("a legal cut");
}

fn rounds(log: &mut AgentLog, n: u64) {
    for k in 1..=n {
        log.append_user(format!("prompt {k}"), None, Voice::Human)
            .unwrap();
        log.append_assistant(ChatMessage::assistant(format!("reply {k}")), vec![], None)
            .unwrap();
    }
}

fn wire(context: &Context) -> Vec<u8> {
    serde_json::to_vec(&context.rendered()).expect("messages serialise")
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a test pattern is a regex")
}

/// An item as a test spells it: a turn by its id, a residue by its span.
#[derive(Debug, PartialEq, Eq)]
enum Shown {
    T(u64),
    R(u64, u64),
}

use Shown::{R, T};

impl Shown {
    fn of(item: &Item<'_>) -> Self {
        match item {
            Item::Turn(turn) => T(turn.id),
            Item::Residue(standing) => R(standing.residue.span.from(), standing.residue.span.to()),
        }
    }

    fn row(row: &SpanRow) -> Self {
        match row.kind {
            ItemKind::Residue => R(row.span.from(), row.span.to()),
            ItemKind::Turn(_) => T(row.span.from()),
        }
    }
}

fn frontier(context: &Context) -> Vec<Shown> {
    context.table.frontier().iter().map(Shown::of).collect()
}

fn index(context: &Context) -> Vec<(Shown, Location)> {
    context
        .transcript_index()
        .iter()
        .map(|row| (Shown::row(row), row.location))
        .collect()
}

fn locations(context: &Context) -> Vec<(u64, Location)> {
    context
        .table
        .turns()
        .iter()
        .map(|turn| (turn.id, turn.location()))
        .collect()
}

fn texts(context: &Context) -> Vec<String> {
    context
        .rendered()
        .iter()
        .map(|message| message.content.first_text().unwrap_or_default().to_string())
        .collect()
}

fn called(desc: Option<&str>) -> ChatMessage {
    let mut args = serde_json::json!({ "cmd": "ls" });
    if let Some(desc) = desc {
        args["description"] = serde_json::json!(desc);
    }
    ChatMessage::assistant(vec![ContentPart::ToolCall(crate::provider::ToolCall {
        call_id: "t1".into(),
        fn_name: "ral".into(),
        fn_arguments: args,
        thought_signatures: None,
    })])
}

/// A span is never inverted: neither constructed nor read back so.
#[test]
fn an_inverted_span_is_refused() {
    assert_eq!(
        Span::new(4, 3).unwrap_err().to_string(),
        "`from` is 4 and `to` is 3: a run names its first turn, then its last"
    );
    assert_eq!(
        serde_json::from_str::<Span>(r#"{"from":3,"to":4}"#).expect("a span"),
        span(3, 4)
    );
    assert!(serde_json::from_str::<Span>(r#"{"from":4,"to":3}"#).is_err());
}

/// A call-first message still states what it is for: where no prose opened
/// the turn, the declared intent is its label.
#[test]
fn a_call_first_message_takes_its_label_from_the_declared_intent() {
    assert_eq!(
        message_label(&called(Some("sweep the tree"))),
        "sweep the tree"
    );
}

/// Prose keeps precedence, and a call that declared nothing leaves the label
/// exactly as empty as before.
#[test]
fn prose_beats_intent_and_an_undeclared_call_stays_unlabelled() {
    assert_eq!(message_label(&called(None)), "");
    let prose = ChatMessage::assistant("now the write\nsecond line".to_string());
    assert_eq!(message_label(&prose), "now the write");
}

#[test]
fn an_eviction_leaves_a_residue_over_its_span() {
    let mut context = exchanges(3);
    evict(&mut context, 3, 4, Some("the lexer was a dead end")).expect("a run of closed turns");
    assert_eq!(frontier(&context), [T(1), T(2), R(3, 4), T(5), T(6)]);
    assert_eq!(
        locations(&context)[2..4],
        [(3, under(3, 4)), (4, under(3, 4))]
    );
    let rendered = context.rendered();
    assert_eq!(rendered.len(), 5);
    assert_eq!(rendered[2].role, ChatRole::User);
    let text = rendered[2].content.first_text().unwrap_or_default();
    assert!(text.contains("Turns 3–4 have left your context"), "{text}");
    assert!(
        text.contains("Your note: \"the lexer was a dead end\""),
        "{text}"
    );
    assert!(text.contains("!{range 3 5}"), "{text}");
    let turns: usize = [1, 2, 5, 6]
        .iter()
        .map(|&id| context.turn(id).expect("recorded").bytes)
        .sum();
    assert_eq!(
        context.history_bytes(),
        turns + message_bytes(&rendered[2..3])
    );
}

#[test]
fn a_larger_run_nests_the_residue() {
    let mut context = exchanges(3);
    evict(&mut context, 3, 4, Some("inner")).expect("a run of closed turns");
    evict(&mut context, 2, 5, Some("outer")).expect("a run covering a residue whole");
    assert_eq!(frontier(&context), [T(1), R(2, 5), T(6)]);
    let residues = context.residues();
    let [inner, outer] = residues.as_slice() else {
        panic!("two evictions, two residues")
    };
    assert!(inner.span.within(outer.span), "the residues are laminar");
    assert_eq!(
        holder(Span::single(3), residues.iter().map(|r| r.span)),
        Some(span(3, 4))
    );
    assert_eq!(
        index(&context),
        [
            (T(1), Location::Resident),
            (R(2, 5), Location::Resident),
            (T(2), under(2, 5)),
            (R(3, 4), under(2, 5)),
            (T(3), under(3, 4)),
            (T(4), under(3, 4)),
            (T(5), under(2, 5)),
            (T(6), Location::Resident),
        ]
    );
}

#[test]
fn a_run_over_one_residue_rewrites_its_note() {
    let mut context = exchanges(3);
    evict(&mut context, 3, 4, Some("first thoughts")).expect("a run of closed turns");
    evict(&mut context, 3, 4, Some("second thoughts")).expect("the same span over a residue");
    assert_eq!(frontier(&context), [T(1), T(2), R(3, 4), T(5), T(6)]);
    let text = &texts(&context)[2];
    assert!(
        text.contains("second thoughts") && !text.contains("first thoughts"),
        "{text}"
    );
    assert_eq!(
        context
            .transcript_index()
            .into_iter()
            .filter(|row| row.kind == ItemKind::Residue)
            .map(|row| (row.label, row.location))
            .collect::<Vec<_>>(),
        [
            ("second thoughts".to_string(), Location::Resident),
            ("first thoughts".to_string(), under(3, 4)),
        ]
    );
}

#[test]
fn a_run_cutting_a_residue_is_refused() {
    let mut context = exchanges(3);
    evict(&mut context, 3, 4, Some("held together")).expect("a run of closed turns");
    for (from, to) in [(4, 5), (2, 3)] {
        assert_eq!(
            context.resolve_cut(span(from, to)).unwrap_err(),
            "turns 3–4 stand together under one note: name from: 3, to: 4 whole, or leave it"
        );
    }
}

#[test]
fn a_prompt_may_leave_alone() {
    let mut context = Context::new(PathBuf::from("record.jsonl"));
    prompt(&mut context, "where is the duplicate flag?");
    call(&mut context, "lexer.rs: none");
    reply(&mut context, "in the parser");
    evict(
        &mut context,
        1,
        1,
        Some("asked where the duplicate flag is"),
    )
    .expect("a prompt may leave under a note");
    assert_eq!(frontier(&context), [R(1, 1), T(2), T(3)]);
    let rendered = context.rendered();
    assert_eq!(rendered[0].role, ChatRole::User);
    let text = rendered[0].content.first_text().unwrap_or_default();
    assert!(
        text.starts_with("[EXARCH // Turn 1 has left your context."),
        "{text}"
    );
}

/// Old turns under a note, then the live prompt and its two answers.
fn under_a_note() -> Context {
    let mut turns: Vec<Turn> = (1..=40)
        .map(|id| there(id, span(1, 40), Role::Assistant, "", 10))
        .collect();
    turns.extend([
        here(41, Role::User, "", 100),
        here(42, Role::Assistant, "", 100),
        here(43, Role::Assistant, "", 50),
    ]);
    context(turns, vec![residue(1, 40, Some("read the lexer"))])
}

#[test]
fn plan_eviction_is_a_prefix_over_the_frontier() {
    let context = under_a_note();
    assert_eq!(context.current_prompt(), Some(41));
    assert_eq!(
        context.plan_forced_eviction(200),
        Some(span(1, 41)),
        "the live prompt is charged and taken"
    );
    assert_eq!(
        context.plan_forced_eviction(0),
        Some(span(1, 42)),
        "never the newest"
    );
    assert_eq!(context.plan_forced_eviction(1000), None);
    context
        .resolve_cut(span(41, 41))
        .expect("the model may evict the live prompt");
}

/// A prefix that is one residue alone would rewrite it with no note, saving
/// nothing: no plan.
#[test]
fn a_lone_residue_is_never_planned() {
    let context = under_a_note();
    assert_eq!(context.plan_forced_eviction(249), Some(span(1, 41)));
    assert_eq!(
        context.plan_forced_eviction(250),
        None,
        "only the residue overflows"
    );
}

/// Pressure at the next boundary finds only the harness's own residue old
/// enough to shed, and appends no second one.
#[test]
fn a_forced_eviction_never_rewrites_its_own_residue() {
    let mut context = exchanges(2);
    assert_eq!(context.plan_forced_eviction(0), Some(span(1, 3)));
    evict(&mut context, 1, 3, None).expect("a forced eviction");
    assert_eq!(frontier(&context), [R(1, 3), T(4)]);
    assert_eq!(context.plan_forced_eviction(0), None);
    assert_eq!(context.residues().len(), 1);
}

#[test]
fn forced_eviction_renders_without_a_note() {
    let mut context = exchanges(2);
    evict(&mut context, 1, 2, None).expect("a forced eviction");
    assert_eq!(
        texts(&context)[0],
        "[EXARCH // Turns 1–2 have left your context in a forced eviction, without a note.\nThey are still readable: exarch-transcript `read [turns: !{range 1 3}] reads them back as material, exarch-transcript `index lists every turn and every note, and exarch-transcript `grep [pattern: 're', turns: `all] searches both.]"
    );
    assert_eq!(
        context.context_survey().rows[0].label,
        "forced eviction, without a note"
    );
    evict(&mut context, 3, 3, None).expect("one turn");
    let text = &texts(&context)[1];
    assert!(
        text.starts_with(
            "[EXARCH // Turn 3 has left your context in a forced eviction, without a note.\nIt is still readable: exarch-transcript `read [turns: !{range 3 4}] reads it back"
        ),
        "{text}"
    );
}

/// A withdrawal is by id, keeps every row and residue, leaves no residue
/// standing for what it took, and mints no id twice; its anchor starts an
/// item.
#[test]
fn rewind_withdraws_by_id_and_refuses_an_anchor_inside_a_residue() {
    let mut context = context(
        vec![
            there(1, span(1, 2), Role::User, "one", 10),
            there(2, span(1, 2), Role::Assistant, "", 10),
            here(3, Role::User, "three", 10),
            here(4, Role::Assistant, "", 10),
            there(5, span(5, 6), Role::User, "five", 10),
            there(6, span(5, 6), Role::Assistant, "", 10),
        ],
        vec![residue(1, 2, Some("first")), residue(5, 6, None)],
    );
    assert_eq!(
        context.rewindable(9).unwrap_err(),
        "turn 9 is not recorded: the latest is 6"
    );
    assert_eq!(
        context.rewindable(6).unwrap_err(),
        "turn 6 lies under the residue over turns 5–6: rewind to turn 5 or turn 7"
    );
    assert!(matches!(
        step(&mut context, Protocol::Rewound { anchor: 2 }),
        Err(Refusal::Foreign { reason, .. }) if reason.contains("inside the residue over turns 1–2")
    ));
    step(&mut context, Protocol::Rewound { anchor: 5 }).expect("a residue's first turn");
    assert_eq!(
        index(&context),
        [
            (R(1, 2), Location::Resident),
            (T(1), under(1, 2)),
            (T(2), under(1, 2)),
            (T(3), Location::Resident),
            (T(4), Location::Resident),
            (R(5, 6), Location::Withdrawn),
            (T(5), Location::Withdrawn),
            (T(6), Location::Withdrawn),
        ]
    );
    assert_eq!(frontier(&context), [R(1, 2), T(3), T(4)]);
    assert_eq!(context.residues().len(), 2, "residues are never dropped");
    assert_eq!(context.next_id(), 7, "ids stay monotone");
    assert!(context.is_ready());
    assert!(context.token_measure_is_stale(0));
    assert_eq!(
        context.rewindable(6).unwrap_err(),
        "turn 6 was already rewound or cleared away; the earliest you can rewind to is turn 1"
    );
    context
        .rewindable(1)
        .expect("an evicted anchor starting a residue may be rewound to");

    step(&mut context, Protocol::Rewound { anchor: 1 }).expect("an evicted anchor");
    assert!(
        index(&context)
            .iter()
            .all(|(_, location)| *location == Location::Withdrawn)
    );
    assert_eq!(context.rendered().len(), 0, "not even a residue stands");
    assert_eq!(context.next_id(), 7);
    assert_eq!(
        context.rewindable(1).unwrap_err(),
        "turn 1 was already rewound or cleared away; nothing has been said since the last \
         /clear or /rewind"
    );
    for (anchor, said) in [(1, "already withdrawn"), (9, "never recorded")] {
        assert!(
            matches!(
                step(&mut context, Protocol::Rewound { anchor }),
                Err(Refusal::Foreign { reason, .. }) if reason.contains(said)
            ),
            "a rewind to turn {anchor} is foreign"
        );
    }
}

#[test]
fn a_span_may_cover_a_withdrawn_run() {
    let mut context = exchanges(3);
    step(&mut context, Protocol::Rewound { anchor: 4 }).expect("a rewind at rest");
    reply(&mut context, "reply 3, again");
    prompt(&mut context, "prompt 8");
    assert_eq!(
        context.plan_forced_eviction(0),
        Some(span(1, 7)),
        "the harness's prefix spans the withdrawn run"
    );
    evict(&mut context, 1, 7, Some("the first attempt")).expect("a span over a withdrawn run");
    assert_eq!(frontier(&context), [R(1, 7), T(8)]);
    assert_eq!(locations(&context)[4], (5, Location::Withdrawn));
}

#[test]
fn an_inherited_table_keeps_residues() {
    let mut parent = log();
    rounds(&mut parent, 3);
    cut(&mut parent, 3, 4, Some("inner"));
    cut(&mut parent, 2, 5, Some("outer"));
    let Inherited { turns, residues } = parent.inherited_context();
    assert_eq!(
        turns
            .iter()
            .map(|linked| (linked.row.id, linked.row.location))
            .collect::<Vec<_>>(),
        [
            (1, Location::Resident),
            (2, under(2, 5)),
            (3, under(3, 4)),
            (4, under(3, 4)),
            (5, under(2, 5)),
            (6, Location::Resident),
        ]
    );
    let link = |turns, residues| Protocol::Inherited { turns, residues };
    let child = || Context::new(PathBuf::from("child.jsonl"));

    let mut inherited = child();
    step(&mut inherited, link(turns.clone(), residues.clone())).expect("the parent's own link");
    assert_eq!(wire(&inherited), wire(parent.context()));
    let located = |context: &Context| {
        context
            .transcript_index()
            .into_iter()
            .map(|row| (row.span, row.location))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        located(&inherited),
        located(parent.context()),
        "every residue keeps its residency across the link"
    );

    let crossed = [residues.clone(), vec![residue(4, 6, Some("across"))]].concat();
    assert!(matches!(
        step(&mut child(), link(turns.clone(), crossed)),
        Err(Refusal::Foreign { reason, .. }) if reason.contains("no sequence of evictions leaves")
    ));
    let mut contradicted = turns;
    contradicted[2].row.location = under(2, 5);
    assert!(matches!(
        step(&mut child(), link(contradicted, residues)),
        Err(Refusal::Foreign { reason, .. }) if reason.contains("its residues contradict")
    ));
}

#[test]
fn fold_equals_memo_across_nested_evictions() {
    let mut live = log();
    rounds(&mut live, 4);
    cut(&mut live, 3, 4, Some("inner"));
    cut(&mut live, 2, 5, Some("outer"));
    cut(&mut live, 1, 6, None);
    rounds(&mut live, 1);
    let (memo, _) = resume(&live.dir().join("record.jsonl")).expect("replay");
    assert_eq!(wire(&memo), wire(live.context()));
    assert_eq!(memo.history_bytes(), live.context().history_bytes());
    assert_eq!(memo.transcript_index(), live.context().transcript_index());
}

#[test]
fn the_residue_text_is_a_function_of_the_record() {
    let record = residue(3, 4, Some("the lexer was a dead end"));
    assert_eq!(render_residue(&record), render_residue(&record.clone()));
    let rendered = |bytes: usize, label: &str| {
        texts(&context(
            vec![
                there(3, span(3, 4), Role::User, label, bytes),
                there(4, span(3, 4), Role::Assistant, label, bytes),
            ],
            vec![record.clone()],
        ))
    };
    assert_eq!(rendered(10, "one"), rendered(90_000, "another"));
    assert_eq!(rendered(10, "one"), [render_residue(&record)]);
}

#[test]
fn grep_finds_a_note_and_names_its_span() {
    let mut log = log();
    rounds(&mut log, 4);
    cut(&mut log, 3, 4, Some("the parser holds the duplicate flag"));
    cut(&mut log, 5, 6, None);
    let hits = |pattern: &str, turns: Option<&[u64]>| {
        log.context()
            .grep_transcript(&regex(pattern), turns)
            .expect("a legal search")
            .hits
    };
    let noted = |hits: Vec<GrepHit>| {
        hits.into_iter()
            .filter(|hit| hit.kind == ItemKind::Residue)
            .map(|hit| (hit.span.from(), hit.span.to(), hit.line, hit.text))
            .collect::<Vec<_>>()
    };
    let note = || vec![(3, 4, 1, "the parser holds the duplicate flag".to_string())];
    assert_eq!(noted(hits("parser", None)), note());
    assert_eq!(hits("parser", None).len(), 1, "no turn says parser");
    assert_eq!(noted(hits("parser", Some(&[3, 4]))), note());
    assert_eq!(hits("parser", Some(&[3])), Vec::<GrepHit>::new());
    assert_eq!(
        noted(hits(".", None)),
        note(),
        "a forced eviction's residue has no note to search"
    );
}

/// The prompt a turn belongs to is read off role and order.
#[test]
fn a_prompt_is_read_off_role_and_order() {
    let context = context(
        vec![
            here(1, Role::User, "one", 10),
            here(2, Role::Assistant, "", 10),
            here(3, Role::Assistant, "", 10),
            here(4, Role::User, "four", 10),
            here(5, Role::Assistant, "", 10),
            here(6, Role::User, "six", 10),
        ],
        Vec::new(),
    );
    assert_eq!(
        (1..=6).map(|id| context.prompt_of(id)).collect::<Vec<_>>(),
        vec![Some(1), Some(1), Some(1), Some(4), Some(4), Some(6)]
    );
    assert_eq!(context.prompt_of(9), None, "turn 9 is not recorded");
    assert_eq!(context.current_prompt(), Some(6));
    assert_eq!(context.current_turn(), Some(6));
}

/// A log that has minted no id has no newest turn to name.
#[test]
fn an_empty_log_has_no_current_turn() {
    let empty = Context::new(PathBuf::from("record.jsonl"));
    assert_eq!(empty.current_turn(), None);
    assert_eq!(empty.current_prompt(), None);
}

/// An imported message opens a fresh turn in its own voice: the message is
/// the one datum that says whose it was.
#[test]
fn an_import_opens_a_turn_in_its_messages_voice() {
    let mut context = Context::new(PathBuf::from("record.jsonl"));
    step(
        &mut context,
        Protocol::Import {
            turn: 1,
            message: ChatMessage::user("the parent's prompt"),
        },
    )
    .expect("an import at a ready boundary");
    step(
        &mut context,
        Protocol::Import {
            turn: 2,
            message: ChatMessage::assistant("the parent's answer"),
        },
    )
    .expect("an import at a ready boundary");
    assert_eq!(
        context
            .transcript_index()
            .iter()
            .map(|row| (row.span.from(), row.role, row.kind))
            .collect::<Vec<_>>(),
        vec![
            (1, Role::User, ItemKind::Turn(TurnKind::Import)),
            (2, Role::Assistant, ItemKind::Turn(TurnKind::Import)),
        ]
    );
    assert_eq!(context.current_prompt(), Some(1));
    assert_eq!(context.current_turn(), Some(2));
}

/// The one turn no eviction takes is the one being written.
#[test]
fn a_cut_refuses_the_unclosed_turn() {
    let mut context = context(
        vec![
            here(1, Role::User, "one", 10),
            here(2, Role::Assistant, "", 10),
        ],
        Vec::new(),
    );
    context.state = State::AwaitingAssistantAfterUser;
    assert_eq!(
        context.resolve_cut(span(2, 2)).unwrap_err(),
        "turn 2 is being written now: an eviction keeps the work in hand"
    );
    assert_eq!(
        context.resolve_cut(span(9, 9)).unwrap_err(),
        "turn 9 is not recorded: the latest is 2"
    );
}

/// A detour inside the work in hand: the good work stands, the detour
/// leaves under one residue, and the turn in hand is untouched.
#[test]
fn a_cut_inside_the_work_in_hand_leaves_one_residue() {
    let mut context = context(
        vec![
            here(30, Role::User, "fix the parser", 10),
            here(31, Role::Assistant, "", 10),
            here(32, Role::Assistant, "", 10),
            here(33, Role::Assistant, "", 10),
            here(34, Role::Assistant, "", 20 * 1024),
            here(35, Role::Assistant, "", 20 * 1024),
            here(36, Role::Assistant, "", 20 * 1024),
            here(37, Role::Assistant, "", 10),
        ],
        Vec::new(),
    );
    context.state = State::AwaitingAssistantAfterUser;
    evict(&mut context, 34, 36, Some("that read was a dead end")).expect("a legal cut");
    assert_eq!(
        frontier(&context),
        [T(30), T(31), T(32), T(33), R(34, 36), T(37)]
    );
}

/// Two residues with a resident turn between them render two messages, each
/// at its own place, and both weigh and count.
#[test]
fn two_residues_stand_in_place_and_weigh_and_count() {
    let context = context(
        vec![
            here(1, Role::User, "one", 10),
            there(2, span(2, 2), Role::Assistant, "", 4 * 1024),
            here(3, Role::User, "three", 10),
            there(4, span(4, 4), Role::Assistant, "", 8 * 1024),
            here(5, Role::User, "five", 10),
        ],
        vec![residue(2, 2, None), residue(4, 4, None)],
    );
    assert_eq!(frontier(&context), [T(1), R(2, 2), T(3), R(4, 4), T(5)]);
    assert_eq!(
        context.event_count(),
        2,
        "no resident turn holds a record here, so the count is the two residues"
    );
    let rendered = context.rendered();
    assert_eq!(
        rendered.len(),
        2,
        "the resident turns hold no records, so only the residues render"
    );
    assert_eq!(
        context.history_bytes(),
        message_bytes(&rendered) + 30,
        "each residue weighs beside the resident turns' own bytes"
    );
}

/// A cut at a ready boundary may take the whole context, and the residue
/// then stands alone.
#[test]
fn a_cut_may_take_the_whole_context() {
    let mut context = context(
        vec![
            here(1, Role::User, "one", 10),
            here(2, Role::Assistant, "", 10),
            here(3, Role::User, "three", 10),
        ],
        Vec::new(),
    );
    evict(&mut context, 1, 3, None).expect("a cut at rest");
    assert_eq!(frontier(&context), [R(1, 3)]);
    assert_eq!(context.rendered().len(), 1, "the residue stands alone");
}

/// A clear withdraws every turn, keeps the system prompt, and waits for the
/// batch in flight.
#[test]
fn a_clear_withdraws_every_turn_and_keeps_the_system_prompt() {
    let mut context = context(
        vec![
            there(1, span(1, 1), Role::User, "one", 10),
            here(2, Role::Assistant, "", 10),
            here(3, Role::User, "three", 10),
        ],
        vec![residue(1, 1, None)],
    );
    step(
        &mut context,
        Protocol::System {
            text: "be brief".into(),
        },
    )
    .expect("a system prompt");
    context.state = State::AwaitingToolResults {
        pending_ids: vec!["call".into()],
    };
    assert!(
        matches!(
            step(&mut context, Protocol::Cleared),
            Err(Refusal::Foreign { .. })
        ),
        "a clear is refused mid-batch"
    );
    context.state = State::default();
    step(&mut context, Protocol::Cleared).expect("a clear at rest");
    assert!(
        index(&context)
            .iter()
            .all(|(_, location)| *location == Location::Withdrawn)
    );
    assert_eq!(&**context.system(), "be brief");
    assert_eq!(context.state, State::default());
    assert_eq!(context.rendered().len(), 0);
    assert_eq!(context.next_id(), 4);
}

/// `System` is folded, and neither opens a turn nor moves the automaton.
#[test]
fn a_system_prompt_is_folded_and_sequencing_neutral() {
    let mut context = Context::new(PathBuf::from("record.jsonl"));
    assert_eq!(&**context.system(), "");
    context.state = State::AwaitingToolResults {
        pending_ids: vec!["call".into()],
    };
    step(
        &mut context,
        Protocol::System {
            text: "first".into(),
        },
    )
    .expect("mid-batch");
    step(
        &mut context,
        Protocol::System {
            text: "second".into(),
        },
    )
    .expect("a later prompt");
    assert_eq!(&**context.system(), "second");
    assert_eq!(context.transcript_index(), Vec::<SpanRow>::new());
    assert_eq!(
        context.state,
        State::AwaitingToolResults {
            pending_ids: vec!["call".into()]
        }
    );
    assert_eq!(
        context.rendered().len(),
        0,
        "the prompt travels beside the transcript"
    );
}

/// A stale import is refused; a fresh one opens a turn under its own id.
#[test]
fn an_import_must_be_fresh() {
    let mut context = context(
        vec![
            here(1, Role::User, "one", 10),
            here(2, Role::Assistant, "", 10),
        ],
        Vec::new(),
    );
    assert!(matches!(
        step(&mut context, Protocol::Import { turn: 2, message: ChatMessage::user("again") }),
        Err(Refusal::Foreign { reason, .. }) if reason.contains("moved past")
    ));
    step(
        &mut context,
        Protocol::Import {
            turn: 3,
            message: ChatMessage::user("fresh"),
        },
    )
    .expect("a fresh import");
    assert_eq!(context.current_turn(), Some(3));
}

/// A forced eviction may take the prompt, but never the newest turn, even
/// when the budget is zero.
#[test]
fn a_forced_eviction_may_take_the_prompt_but_not_the_newest_turn() {
    let context = context(
        vec![
            here(1, Role::User, "one", 100),
            here(2, Role::Assistant, "", 100),
        ],
        Vec::new(),
    );
    assert_eq!(context.plan_forced_eviction(0), Some(span(1, 1)));
}

/// A link whose parent's file will not read back is refused as unreadable,
/// by the file's name, and leaves the structure as it was.
#[test]
fn an_unreadable_parent_refuses_the_link_and_changes_nothing() {
    let source = PathBuf::from("/nonexistent/parent/record.jsonl");
    let mut context = Context::new(PathBuf::from("record.jsonl"));
    let link = Protocol::Inherited {
        turns: vec![Linked {
            row: TurnRow {
                id: 1,
                role: Role::User,
                kind: TurnKind::Inherited,
                label: "one".into(),
                bytes: 10,
                location: Location::Resident,
            },
            at: Pointer {
                source: source.clone(),
                loci: vec![Locus::placeholder(Seq::new(1))],
            },
        }],
        residues: Vec::new(),
    };
    let Err(Refusal::Unreadable(error)) = step(&mut context, link) else {
        panic!("a missing parent file is unreadable")
    };
    assert!(
        error.to_string().contains(&source.display().to_string()),
        "{error}"
    );
    assert_eq!(context.log_len(), 0);
    assert_eq!(context.transcript_index(), Vec::<SpanRow>::new());
}
