use super::BlockId;
use super::*;
use crate::provider::Tuning;

/// A block that never joins the one before it, so a test about the window
/// counts blocks rather than the lanes that grow.
fn push(memo: &mut Blocks, seq: u64, text: &str) -> Delta {
    memo.step_forensic(Seq::new(seq), &Forensic::SystemNote { text: text.into() })
}

/// A rewind cuts the view back to the first block that is a turn at or
/// past the anchor; a steering line opens no turn and never anchors one.
#[test]
fn a_rewind_cuts_from_the_anchors_first_block() {
    let prompt = |text: &str, turn| match turn {
        Some(turn) => Protocol::UserPrompt {
            turn,
            text: text.into(),
            voice: Voice::Human,
        },
        None => Protocol::Steering {
            text: text.into(),
            voice: Voice::Human,
        },
    };
    let started = |turn| Forensic::TurnStarted {
        turn,
        tuning: Tuning::default(),
        route: None,
    };
    let answer = |text: &str| Display::Answer { text: text.into() };
    let mut memo = Blocks::default();
    let _ = memo.step_protocol(Seq::new(1), &prompt("one", Some(1)));
    let _ = memo.step_forensic(Seq::new(2), &started(2));
    let _ = memo.step_display(Seq::new(3), answer("two"));
    let _ = memo.step_protocol(Seq::new(4), &prompt("three", Some(3)));
    let _ = memo.step_forensic(Seq::new(5), &started(4));
    let _ = memo.step_protocol(Seq::new(6), &prompt("steer", None));
    let _ = memo.step_forensic(Seq::new(7), &started(5));
    let _ = memo.step_display(Seq::new(8), answer("four"));
    let _ = memo.step_protocol(Seq::new(9), &prompt("five", Some(6)));
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
        memo.step_protocol(Seq::new(10), &Protocol::Rewound { anchor: 3 }),
        Delta::Rewound(Some(BlockId::new(Seq::new(4))))
    );
    assert!(matches!(
        memo.blocks(),
        [_, _, _, b] if matches!(b.kind, BlockKind::Rewound { anchor: 3 })
    ));
    assert_eq!(
        memo.step_protocol(Seq::new(11), &Protocol::Rewound { anchor: 1 }),
        Delta::Rewound(Some(BlockId::new(Seq::new(1))))
    );
    assert_eq!(memo.blocks().len(), 1);
}

/// Only a voice the screen does not already speak for draws a prompt; a
/// steering line draws as a prompt that opens no turn.
#[test]
fn a_prompt_is_drawn_unless_the_harness_spoke_it() {
    for (voice, drawn) in [
        (Voice::Human, true),
        (Voice::Wakeup, true),
        (Voice::Peer, true),
        (Voice::Harness, false),
    ] {
        let mut memo = Blocks::default();
        let prompt = Protocol::UserPrompt {
            turn: 1,
            text: "hi".into(),
            voice,
        };
        let steering = Protocol::Steering {
            text: "also".into(),
            voice,
        };
        assert_eq!(
            memo.step_protocol(Seq::new(1), &prompt) != Delta::Quiet,
            drawn,
            "{voice:?} prompt"
        );
        assert_eq!(
            memo.step_protocol(Seq::new(2), &steering) != Delta::Quiet,
            drawn,
            "{voice:?} steering"
        );
        assert_eq!(memo.blocks().len(), if drawn { 2 } else { 0 });
        if drawn {
            assert!(matches!(
                memo.blocks(),
                [first, second]
                    if matches!(first.kind, BlockKind::Prompt { turn: Some(1), .. })
                        && matches!(second.kind, BlockKind::Prompt { turn: None, .. })
            ));
        }
    }
}

/// The model's own eviction is drawn by its act row, so only the harness's
/// draws a marker.
#[test]
fn only_a_harness_eviction_draws() {
    let evicted = |by| Protocol::Evicted {
        cut: Cut {
            turns: vec![1],
            note: None,
        },
        by,
    };
    let mut memo = Blocks::default();
    assert_eq!(
        memo.step_protocol(Seq::new(1), &evicted(EditAuthority::Model)),
        Delta::Quiet
    );
    assert_eq!(
        memo.step_protocol(Seq::new(2), &evicted(EditAuthority::Harness)),
        Delta::Opened(BlockId::new(Seq::new(2)))
    );
    assert!(matches!(
        memo.blocks(),
        [b] if matches!(b.kind, BlockKind::Evicted { by: EditAuthority::Harness, .. })
    ));
}

/// A clear drops every block and leaves only its own marker, but the totals
/// the context gauge reads off usage survive it.
#[test]
fn a_clear_leaves_its_marker_and_the_usage() {
    let mut memo = Blocks::default();
    let _ = memo.step_display(
        Seq::new(1),
        Display::Answer {
            text: "old\n".into(),
        },
    );
    let _ = memo.step_forensic(
        Seq::new(2),
        &Forensic::UsageDelta {
            usage: Usage {
                input: 100,
                ..Usage::default()
            },
            generation: None,
            provider: None,
        },
    );
    assert_eq!(
        memo.step_protocol(Seq::new(3), &Protocol::Cleared),
        Delta::Cleared
    );
    assert!(matches!(
        memo.blocks(),
        [b] if matches!(b.kind, BlockKind::Cleared) && b.seq == Seq::new(3)
    ));
    assert_eq!(memo.origin(), Some(Seq::new(3)));
    assert_eq!(memo.usage().input, 100, "usage persists");
    assert_eq!(memo.last_input(), 0, "the gauge numerator restarts");
}

/// A request's breadcrumb is the turn id it will produce.
#[test]
fn a_started_turn_opens_a_turn_block() {
    let mut memo = Blocks::default();
    let delta = memo.step_forensic(
        Seq::new(1),
        &Forensic::TurnStarted {
            turn: 7,
            tuning: Tuning::default(),
            route: None,
        },
    );
    assert_eq!(delta, Delta::Opened(BlockId::new(Seq::new(1))));
    assert!(matches!(
        memo.blocks(),
        [b] if matches!(b.kind, BlockKind::Turn { id: 7 })
    ));
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

/// A verdict addresses its call wherever that call sits, and a patch whose
/// target the window has let go moves nothing.
#[test]
fn a_verdict_patches_the_call_it_names() {
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
    let _ = memo.step_protocol(
        Seq::new(2),
        &Protocol::UserPrompt {
            turn: 1,
            text: "on".into(),
            voice: Voice::Human,
        },
    );
    assert_eq!(
        memo.step_display(
            Seq::new(3),
            Display::Verdict {
                call,
                verdict: Verdict::of("a\nb\n", false),
            },
        ),
        Delta::Patched(call)
    );
    assert!(matches!(
        memo.blocks()[0].kind(),
        BlockKind::ToolCall {
            verdict: Some(Verdict {
                lines: 2,
                failed: false
            }),
            ..
        }
    ));
    assert_eq!(
        memo.step_display(
            Seq::new(4),
            Display::Verdict {
                call: BlockId::new(Seq::new(99)),
                verdict: Verdict::of("a\n", false),
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
        service: "s".into(),
        account: "a".into(),
    };
    let mut memo = Blocks::default();
    let _ = memo.step_forensic(Seq::new(1), &change(Some(200_000)));
    assert_eq!(memo.context_window(), Some(200_000));
    let _ = memo.step_forensic(Seq::new(2), &change(None));
    assert_eq!(memo.context_window(), None);
}

#[test]
fn last_input_is_the_latest_turn_while_usage_sums() {
    let delta = |input| Forensic::UsageDelta {
        usage: Usage {
            input,
            ..Usage::default()
        },
        generation: None,
        provider: None,
    };
    let mut memo = Blocks::default();
    let _ = memo.step_forensic(Seq::new(1), &delta(100));
    let _ = memo.step_forensic(Seq::new(2), &delta(250));
    assert_eq!(memo.last_input(), 250);
    assert_eq!(memo.usage().input, 350);
}
