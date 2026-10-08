use super::*;

fn act(verb: &str, subject: Option<&str>, payload: &str, failed: bool) -> Block {
    Block::new(
        BlockKind::act(
            verb.into(),
            subject.map(str::to_string),
            payload.into(),
            failed,
        ),
        None,
    )
}

fn thinking(text: &str) -> Block {
    Block::group(Member::Thinking(text.into()), Detail::Full, None)
}

fn run(intent: &str) -> Block {
    Block::group(
        Member::Call(group::Call::open(
            Seq::new(1),
            intent.into(),
            "read 'x'".into(),
            0,
        )),
        Detail::Full,
        None,
    )
}

/// Prose has no summary to collapse to: every rung renders the same.
#[test]
fn prose_is_inert() {
    let block = Block::prose(
        "# heading\n\nA paragraph of prose that the answer is to read.".into(),
        Fidelity::default(),
        false,
        None,
    );
    assert!(!block.dialable(Part::Run) && !block.dialable(Part::Thinking));
    let full = block.body(READ_W, Detail::Full, "");
    assert_eq!(block.body(READ_W, Detail::Summary, ""), full);
    assert_eq!(block.body(READ_W, Detail::Tally, ""), full);
}

/// A continuing paragraph keeps the margin and drops the glyph, so one
/// response wears one `·`.
#[test]
fn a_continuing_paragraph_drops_the_rail_mark() {
    let gutters = |continues| {
        Block::prose(
            "more of the answer".into(),
            Fidelity::default(),
            continues,
            None,
        )
        .railed(READ_W, AgentSlot(0), None, "")
        .0
        .iter()
        .map(|r| r.gutter().to_owned())
        .collect::<Vec<_>>()
    };
    assert!(
        gutters(false).iter().any(|g| g.trim() == "·"),
        "the head of a run wears the mark"
    );
    assert!(
        gutters(true).iter().all(|g| g.trim().is_empty()),
        "and a continuation wears the margin alone"
    );
}

/// An act's verb column is pinned, so verbs align down the page — the
/// alignment `render_field_rows` cannot supply, since each act is a row that
/// would only align with itself.  The subject is not a column: it follows
/// its verb, whole, and the payload follows it.
#[test]
fn an_act_pins_its_verb_column_and_nothing_else() {
    let rendered = |verb, subject: Option<&str>, payload: &str| {
        let block = act(verb, subject, payload, false);
        let lines = block.body(READ_W, Detail::Summary, "");
        line::text(lines.last().expect("an act renders one content row"))
    };
    assert_eq!(
        rendered(
            "spawn",
            Some("hunter"),
            "audit every unwrap() in exarch/src"
        ),
        "spawn      [hunter] audit every unwrap() in exarch/src"
    );
    assert_eq!(
        rendered("unschedule", Some("nightly"), ""),
        "unschedule [nightly]",
        "a landed act with no argument leaves the payload cell empty"
    );
    assert_eq!(
        rendered("schedule", Some("nightly"), "0 9 * * 1-5"),
        "schedule   [nightly] 0 9 * * 1-5"
    );
    assert_eq!(
        rendered("reply", None, "[status: \"clean\", findings: 0]"),
        "reply      [status: \"clean\", findings: 0]",
        "a subject-less act opens its payload at the verb column"
    );
    assert_eq!(
        rendered("unschedule", Some("hunter"), "3 turns"),
        "unschedule [hunter] 3 turns",
        "the longest verb still clears its column by a space"
    );
    assert_eq!(
        rendered("spawn", Some("a-name-of-the-full-24-ch"), "go"),
        "spawn      [a-name-of-the-full-24-ch] go",
        "a name is an identity, and an identity is never cut"
    );
}

/// A reply's payload is a ral value, not a sentence, so it reads in the
/// language's own colours — the same lexer the tool-call panels use.
#[test]
fn a_reply_reads_its_value_in_ral() {
    let block = act("reply", None, "[status: \"clean\", findings: 0]", false);
    let lines = block.body(READ_W, Detail::Summary, "");
    let row = lines.last().expect("an act renders one content row");
    let string = row
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "\"clean\"")
        .expect("the value's string literal is a span of its own");
    assert_eq!(string.style.fg, Some(super::super::palette::CODE_STRING));

    // A refusal is prose whatever the verb, and stays hot rather than lexed.
    let refused = act(
        "spawn",
        Some("hunter"),
        "refused: [that name is taken]",
        true,
    );
    let refused = refused.body(READ_W, Detail::Summary, "");
    assert_eq!(
        refused
            .last()
            .expect("a row")
            .spans
            .last()
            .expect("payload")
            .style
            .fg,
        Some(super::super::palette::RED_HOT)
    );
}

/// An act changes the world; it does not measure it.  So: no magnitude and
/// no size-bar.
#[test]
fn an_act_carries_no_magnitude_and_no_bar() {
    let block = act("message", Some("hunter"), "focus on it", false);
    assert!(block.magnitude().is_none(), "an act ranks nothing");
    for at in [Detail::Summary, Detail::Full] {
        let text: String = block.body(READ_W, at, "").iter().map(line::text).collect();
        assert!(
            !text.contains('\u{2588}') && !text.contains('\u{2591}'),
            "no size-bar on an act row at {at:?}: {text:?}"
        );
    }
}

/// A refusal reads hot on the very row that names the attempt; the long
/// form is the raise, and the raise is the model's.
#[test]
fn a_refused_act_tiers_its_outcome_hot() {
    let block = act("cancel", Some("hunter"), "refused: not a descendant", true);
    let lines = block.body(READ_W, Detail::Summary, "");
    let row = lines.last().expect("an act renders one content row");
    assert_eq!(
        line::text(row),
        "cancel     [hunter] refused: not a descendant"
    );
    let outcome = row.spans.last().expect("the payload span");
    assert_eq!(outcome.style.fg, Some(super::super::palette::RED_HOT));
    assert!(outcome.style.add_modifier.contains(Modifier::BOLD));

    // A landed act of the same verb wears the ordinary body ink.
    let landed = act(
        "cancel",
        Some("hunter"),
        "no live agent by that name",
        false,
    );
    let landed = landed.body(READ_W, Detail::Summary, "");
    assert_eq!(
        landed
            .last()
            .expect("a row")
            .spans
            .last()
            .expect("payload")
            .style
            .fg,
        Some(SLATE)
    );
}

/// Reduced, the payload is cut to its column; the dial is what gets the rest
/// back, wrapped and hanging at the head row's own offset.
#[test]
fn a_long_payload_truncates_reduced_and_returns_whole_on_the_dial() {
    let payload = "audit every unwrap() in exarch/src and report the ones that can \
        actually fire, with the file and line and a one-sentence argument for each";
    let block = act("spawn", Some("hunter"), payload, false);
    assert!(
        block.dialable(Part::Run),
        "the dial is what keeps the rest reachable"
    );

    let reduced = block.body(READ_W, Detail::Summary, "");
    assert_eq!(reduced.len(), 2, "reduced, an act is one row and its blank");
    let head = line::text(&reduced[1]);
    assert!(
        head.ends_with('\u{2026}'),
        "the cut payload ends in an ellipsis: {head:?}"
    );
    assert!(
        head.chars().count() < payload.chars().count(),
        "the payload was cut to its column"
    );

    let full: String = block
        .body(READ_W, Detail::Full, "")
        .iter()
        .map(|l| line::text(l).trim_start().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    for word in ["one-sentence", "argument", "for", "each"] {
        assert!(
            full.contains(word),
            "the whole payload returns on the dial: {full:?}"
        );
    }
}

/// An act is a barrier wearing its own shape, and the shape says when it
/// lands.
#[test]
fn acts_wear_their_own_shapes() {
    for (verb, shape) in [
        ("spawn", RailKind::FleetAct),
        ("cancel", RailKind::FleetAct),
        ("message", RailKind::FleetAct),
        ("reply", RailKind::FleetAct),
        ("schedule", RailKind::TimeAct),
        ("unschedule", RailKind::TimeAct),
    ] {
        let block = act(verb, Some("subject"), "payload", false);
        for at in [Detail::Summary, Detail::Full] {
            assert_eq!(block.rail_kind(), Some(shape), "`{verb}` at {at:?}");
        }
    }
}

#[test]
fn opening_chrome_has_no_rail() {
    let block = Block::chrome(Chrome::Opening(Card(Vec::new())), None);
    assert_eq!(block.rail_kind(), None);
}

/// A deliberation is one thing, shown whole or as its header, so its dial
/// hops between two rungs; a run alone reaches the tally floor, and the
/// two dials on one group move apart.
#[test]
fn each_part_walks_its_own_rungs() {
    let mut lone = thinking("weighing it");
    assert!(lone.dialable(Part::Thinking) && !lone.dialable(Part::Run));
    assert!(lone.cycle(Part::Thinking));

    let mut group = run("read it");
    assert!(group.dialable(Part::Run) && !group.dialable(Part::Thinking));
    let rungs = |b: &Block| match &b.kind {
        BlockKind::Group(g) => (g.thinking.dial.at, g.run.at),
        _ => panic!("a group"),
    };
    assert_eq!(rungs(&group).1, Detail::Summary, "work arrives collapsed");
    assert!(group.cycle(Part::Run));
    assert_eq!(rungs(&group).1, Detail::Full);
    assert!(group.cycle(Part::Run));
    assert_eq!(rungs(&group).1, Detail::Tally, "a run wraps to its tally");
}

/// An open line reads inside the part that will absorb it, and never
/// touches the memo — the record that completes it changes the text and
/// not the picture.
#[test]
fn an_open_line_reads_inside_the_part_it_joins() {
    let mut group = thinking("first thought\n");
    group.fill(READ_W, AgentSlot(0));
    let committed = group.rendered().len();
    let live = group.live(READ_W, AgentSlot(0), "and a second");
    assert_eq!(
        group.rendered().len(),
        committed,
        "drawing the open line left the memo alone"
    );
    assert!(
        live.iter().any(|r| r.plain().contains("and a second")),
        "the open line reads where its part will hold it"
    );
}
