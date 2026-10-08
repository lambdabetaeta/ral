use super::BlockId;
use super::*;

/// A block that never joins the one before it, so a test about the window
/// counts blocks rather than the lanes that grow.
fn push(memo: &mut Blocks, seq: u64, text: &str) -> Delta {
    memo.step_forensic(Seq::new(seq), Forensic::SystemNote { text: text.into() })
}

/// A rewind cuts the view back to the first block that is a turn at or
/// past the anchor; a steering line opens no turn and never anchors one.
#[test]
fn a_rewind_cuts_from_the_anchors_first_block() {
    let prompt = |text: &str, turn| Display::Prompt {
        text: text.into(),
        turn,
    };
    let turn = |id| Display::Turn { id };
    let answer = |text: &str| Display::Answer { text: text.into() };
    let mut memo = Blocks::default();
    for (seq, d) in [
        (1, prompt("one", Some(1))),
        (2, turn(2)),
        (3, answer("two")),
        (4, prompt("three", Some(3))),
        (5, turn(4)),
        (6, prompt("steer", None)),
        (7, turn(5)),
        (8, answer("four")),
        (9, prompt("five", Some(6))),
    ] {
        let _ = memo.step_display(Seq::new(seq), d);
    }
    assert_eq!(
        memo.prompts().collect::<Vec<_>>(),
        [(1, "one"), (3, "three"), (6, "five")],
        "only turn-opening prompts are offered"
    );
    assert_eq!(
        memo.rewind_at(3),
        Some(3),
        "the prompt of turn 3 goes with it"
    );
    assert_eq!(memo.rewind_at(4), Some(4), "turn 3's prompt stays");
    assert_eq!(
        memo.rewind_at(5),
        Some(6),
        "turn 5's breadcrumb, the steering line before it kept"
    );
    assert_eq!(memo.rewind_at(6), Some(8), "a trailing prompt");
    assert_eq!(memo.rewind_at(7), None, "no block is a turn past the last");
    assert_eq!(
        memo.rewind_at(2),
        Some(1),
        "turn 2's own breadcrumb, its prompt kept"
    );
    assert_eq!(
        memo.step_display(Seq::new(10), Display::Rewound { anchor: 3 }),
        Delta::Rewound(Some(BlockId::new(Seq::new(4))))
    );
    assert!(matches!(
        memo.blocks(),
        [_, _, _, b] if matches!(b.kind, BlockKind::Rewound { anchor: 3 })
    ));
    assert_eq!(
        memo.step_display(Seq::new(11), Display::Rewound { anchor: 1 }),
        Delta::Rewound(Some(BlockId::new(Seq::new(1))))
    );
    assert_eq!(memo.blocks().len(), 1);
}

/// One lane, many records, one block: consecutive prose grows the block it
/// opened, and a record of another kind ends the run.
#[test]
fn consecutive_records_of_one_lane_grow_a_single_block() {
    let mut memo = Blocks::default();
    let opened = memo.step_display(
        Seq::new(1),
        Display::Answer {
            text: "first line\n".into(),
        },
    );
    assert_eq!(opened, Delta::Opened(BlockId::new(Seq::new(1))));
    let grew = memo.step_display(
        Seq::new(2),
        Display::Answer {
            text: "second line\n".into(),
        },
    );
    assert_eq!(
        grew,
        Delta::Grew(BlockId::new(Seq::new(1))),
        "the second record grows the block the first opened"
    );
    match memo.blocks() {
        [block] => {
            let BlockKind::Answer { text } = block.kind() else {
                panic!("expected one answer block")
            };
            assert_eq!(text, "first line\nsecond line\n", "the block holds the run");
        }
        other => panic!("expected one block, got {}", other.len()),
    }

    let _ = memo.step_display(
        Seq::new(3),
        Display::ToolCall {
            tool: "ral".into(),
            cmd: "ls".into(),
            summary: None,
        },
    );
    let _ = memo.step_display(
        Seq::new(4),
        Display::Answer {
            text: "after the call\n".into(),
        },
    );
    assert_eq!(
        memo.blocks().len(),
        3,
        "the tool call ends the run, so the prose after it opens its own block"
    );
}

/// A result addresses its call wherever that call sits, and a patch whose
/// target the window has let go moves nothing.
#[test]
fn a_result_patches_the_call_it_names() {
    let mut memo = Blocks::default();
    let call = BlockId::new(Seq::new(1));
    let _ = memo.step_display(
        Seq::new(1),
        Display::ToolCall {
            tool: "ral".into(),
            cmd: "read 'x'".into(),
            summary: Some("look at x".into()),
        },
    );
    let _ = memo.step_display(
        Seq::new(2),
        Display::Prompt {
            text: "on".into(),
            turn: Some(1),
        },
    );
    assert_eq!(
        memo.step_display(
            Seq::new(3),
            Display::Result {
                text: "a\nb\n".into(),
                failed: false,
                call,
            },
        ),
        Delta::Patched(call)
    );
    assert_eq!(
        memo.step_display(
            Seq::new(4),
            Display::Result {
                text: "a\n".into(),
                failed: false,
                call: BlockId::new(Seq::new(99)),
            },
        ),
        Delta::Quiet,
        "a patch naming no resident call moves nothing"
    );
}

#[test]
fn the_window_bounds_the_fold_as_blocks_land() {
    let mut memo = Blocks::default();
    let total = BLOCKS_WINDOW + 50;
    for i in 1..=total {
        let _ = push(&mut memo, i as u64, &format!("block {i}"));
    }
    assert_eq!(
        memo.blocks().len(),
        BLOCKS_WINDOW,
        "the window binds while blocks land, with no flush to wait on"
    );
    assert_eq!(
        memo.blocks().first().map(|b| b.seq),
        Some(Seq::new(51)),
        "the oldest blocks go first"
    );
    assert_eq!(
        memo.origin(),
        Some(Seq::new(1)),
        "the session's opening block is remembered past its eviction"
    );
}

#[test]
fn a_model_change_carries_its_window_and_a_bare_one_clears_it() {
    let change = |window| Forensic::ModelChanged {
        model: "m".into(),
        context_window: window,
        label: "p".into(),
        service: None,
        account: None,
    };
    let mut memo = Blocks::default();
    let _ = memo.step_forensic(Seq::new(1), change(Some(200_000)));
    assert_eq!(memo.context_window(), Some(200_000));
    let _ = memo.step_forensic(Seq::new(2), change(None));
    assert_eq!(memo.context_window(), None);
}
