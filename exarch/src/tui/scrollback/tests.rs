use super::*;
use crate::record::{Display, Locus, Record, Recorded};

fn scrollback() -> Scrollback {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "exarch-scrollback-test-{}-{}.log",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
    ));
    Scrollback::new(path, AgentSlot(0), false, Detail::Full)
}

fn rail_rows(rows: &[Row], glyph: &str) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, row)| (row.gutter() == glyph).then_some(i))
        .collect()
}

/// A running counter's worth of facts, so a test can interleave chrome
/// between commits without two calls colliding on the same `Seq`.
struct Stream(u64);

impl Stream {
    fn new() -> Self {
        Self(0)
    }

    fn land(&mut self, sb: &mut Scrollback, record: Record) -> Seq {
        self.0 += 1;
        let seq = Seq::new(self.0);
        sb.fact(&Recorded::new(Locus::placeholder(seq), record));
        seq
    }

    fn say(&mut self, sb: &mut Scrollback, text: &str) {
        let _ = self.land(sb, Record::Display(Display::Answer { text: text.into() }));
    }

    fn call(&mut self, sb: &mut Scrollback, intent: &str, cmd: &str) -> Seq {
        self.land(
            sb,
            Record::Display(Display::ToolCall {
                tool: "ral".into(),
                cmd: cmd.into(),
                summary: Some(intent.into()),
            }),
        )
    }
}

fn text(sb: &mut Scrollback) -> String {
    sb.render_window(READ_W, 60)
        .lines
        .iter()
        .map(Row::plain)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Re-pinning overwrites a slot in place, `drop_pin` removes one, `reset`
/// wipes the lot — the same generation discipline that bounds scrollback.
#[test]
fn pins_overwrite_in_place_and_keep_insertion_order() {
    use crate::card::Mark;
    let raw = |b: &[u8]| Card(vec![Mark::Raw { bytes: b.to_vec() }]);
    let keys = |sb: &Scrollback| sb.pins().iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    let mut sb = scrollback();
    sb.set_pin("tasks".into(), raw(b"v1"));
    sb.set_pin("build".into(), raw(b"ok"));
    assert_eq!(keys(&sb), ["tasks", "build"]);

    sb.set_pin("tasks".into(), raw(b"v2"));
    assert_eq!(keys(&sb), ["tasks", "build"]);
    assert!(matches!(&sb.pins()[0].1.0[..], [Mark::Raw { bytes }] if bytes == b"v2"));

    sb.drop_pin("tasks");
    assert_eq!(keys(&sb), ["build"]);
    sb.reset();
    assert!(sb.pins().is_empty(), "reset wipes the register");
}

/// The open line reads inside the block it will join, and the record that
/// completes it changes the text without changing the picture — the one
/// claim that says live and committed are the same rendering.
#[test]
fn the_record_that_completes_a_line_does_not_change_the_picture() {
    let mut sb = scrollback();
    let mut log = Stream::new();

    // A fence opens and a line of code streams: the fence is recorded, the
    // line is still open.
    sb.transient(&Transient::Token("```ral\nlet x = 1".into()));
    log.say(&mut sb, "```ral\n");
    let live = format!("{:?}", sb.render_window(READ_W, 24).lines);
    assert!(
        sb.render_window(READ_W, 24)
            .lines
            .last()
            .expect("a row")
            .plain()
            .contains("let x = 1"),
        "the open line reads where its block will hold it"
    );

    // Its record lands and the printer's own newline rule closes it.
    sb.transient(&Transient::Token("\n".into()));
    log.say(&mut sb, "let x = 1\n");
    assert_eq!(
        format!("{:?}", sb.render_window(READ_W, 24).lines),
        live,
        "the record changed the text and not the picture"
    );
}

/// At most one lane is ever open, because prose ends the thinking run on
/// the printer's side exactly as it does on the worker's — and the turn's
/// boundary clears whatever is left.
#[test]
fn prose_ends_the_open_thinking_line_and_the_boundary_clears_both() {
    let mut sb = scrollback();
    sb.transient(&Transient::Thinking("considering the shape".into()));

    let all = text(&mut sb);
    assert!(
        all.contains("considering the shape"),
        "the thinking lane's open line reads on its own rail: {all:?}"
    );
    assert_eq!(
        rail_rows(&sb.render_window(READ_W, 60).lines, "∴ ").len(),
        1,
        "and wears the thinking rail: {all:?}"
    );

    sb.transient(&Transient::Token("First words".into()));
    let all = text(&mut sb);
    assert!(
        !all.contains("considering the shape") && all.contains("First words"),
        "prose closed the run, whose tail the worker recorded at the same delta: {all:?}"
    );

    sb.transient(&Transient::Boundary);
    let all = text(&mut sb);
    assert!(
        !all.contains("First words"),
        "no open line outlives the turn it was read from: {all:?}"
    );
}

/// `scroll_down` clears `sticky`, so `render_window` takes the clamping
/// branch — scrolling down at the bottom must not over-scroll past
/// `max_off` and blank the rows below.
#[test]
fn scroll_down_while_sticky_clamps_to_max_off() {
    let mut sb = scrollback();
    for i in 0..10 {
        sb.push_chrome(Chrome::Prompt(format!(
            "block {i} line a\nblock {i} line b\nblock {i} line c"
        )));
    }
    let height = 10;
    let w0 = sb.render_window(READ_W, height);
    assert!(sb.sticky, "a fresh scrollback follows the tail");
    let max_off = w0.offset;
    sb.scroll_down(5);
    let w1 = sb.render_window(READ_W, height);
    assert_eq!(
        w1.offset, max_off,
        "scroll_down while sticky stays at max_off, not past it"
    );
    assert_eq!(
        w1.lines.len(),
        height,
        "the window fills every row: no blank space below the tail"
    );
    assert!(sb.sticky, "re-armed at the bottom after the clamp");
}

/// The committed deliberation renders in a sticky scrollback too, once its
/// record lands past a full window of chrome.
#[test]
fn committed_thinking_stays_visible_in_sticky_scrollback() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    sb.push_chrome(Chrome::Prompt("hello cutie".into()));
    for i in 0..8 {
        sb.push_chrome(Chrome::Prompt(format!(
            "block {i} line a\nblock {i} line b"
        )));
    }
    sb.transient(&Transient::Thinking("considering the shape".into()));
    let live = sb.render_window(READ_W, 8);
    assert!(
        !rail_rows(&live.lines, "∴ ").is_empty(),
        "live thinking has its rail"
    );

    let _ = log.land(
        &mut sb,
        Record::Display(Display::Thinking {
            text: "considering the shape\n".into(),
        }),
    );
    let committed = sb.render_window(READ_W, 8);
    assert!(
        !rail_rows(&committed.lines, "∴ ").is_empty(),
        "committed thinking stays visible in a sticky scrollback: {:?}",
        committed.lines.iter().map(Row::plain).collect::<Vec<_>>()
    );
}

/// Eviction drops the scrollback and the pinned register, keeping only
/// the fact that the view is dead; `log_path` is untouched, so the log
/// stays readable.
#[test]
fn evict_to_tombstone_drops_scrollback_and_register() {
    let mut sb = scrollback();
    sb.push_chrome(Chrome::Note("hello".into()));
    sb.set_pin("k".into(), Card(Vec::new()));
    assert!(!sb.blocks.is_empty());

    let log_path = sb.log_path.clone();
    sb.evict_to_tombstone();

    assert_eq!(sb.log_path, log_path);
    assert!(sb.blocks.is_empty(), "the scrollback is dropped");
    assert!(sb.pins().is_empty(), "the pinned register is dropped");
}

/// Re-evicting is a harmless no-op: the view is already clean.
#[test]
fn evict_to_tombstone_is_idempotent() {
    let mut sb = scrollback();
    sb.push_chrome(Chrome::Error("boom".into()));
    sb.evict_to_tombstone();
    assert!(sb.blocks.is_empty());
    sb.evict_to_tombstone();
    assert!(sb.blocks.is_empty());
}

/// An act is a barrier: it ends the group before it, so it renders
/// standalone under its own `↗` between two separate `▸` runs, never
/// swallowed into a burst of reads.
#[test]
fn an_act_breaks_a_run_of_observations() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let _ = log.call(&mut sb, "read the renderer", "read 'line.rs'");
    let _ = log.call(&mut sb, "read the block", "read 'block.rs'");
    let _ = log.land(
        &mut sb,
        Record::Display(Display::HarnessCall {
            verb: "message".into(),
            subject: Some("hunter".into()),
            payload: "focus on the renderer first".into(),
            failed: false,
        }),
    );
    let _ = log.call(&mut sb, "read the rail", "read 'rail.rs'");

    let w = sb.render_window(READ_W, 40);
    let act = rail_rows(&w.lines, "↗ ");
    assert_eq!(act.len(), 1, "the act renders exactly one rail row");
    let runs = rail_rows(&w.lines, "▸ ");
    assert_eq!(
        runs.len(),
        2,
        "the act splits the reads into two groups, not one"
    );
    assert!(
        runs[0] < act[0] && act[0] < runs[1],
        "the act holds its arrival position between the two runs"
    );
    // Its own row is three columns, not an intent line under a run's head.
    assert_eq!(
        w.lines[act[0]].plain(),
        "message    [hunter] focus on the renderer first"
    );
}

/// A result addresses the call it names, so a call shielded by a later act
/// still earns the bar its own result measured.
#[test]
fn a_result_stamps_the_call_it_names() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let call = log.call(&mut sb, "read the renderer", "read 'line.rs'");
    let _ = log.land(
        &mut sb,
        Record::Display(Display::HarnessCall {
            verb: "cancel".into(),
            subject: Some("hunter".into()),
            payload: String::new(),
            failed: false,
        }),
    );
    let _ = log.land(
        &mut sb,
        Record::Display(Display::Result {
            text: "a line\n".repeat(40),
            failed: false,
            call: BlockId::new(call),
        }),
    );

    let w = sb.render_window(READ_W, 40);
    let act = rail_rows(&w.lines, "↗ ");
    assert_eq!(w.lines[act[0]].plain(), "cancel     [hunter]");
    let run = w.lines[rail_rows(&w.lines, "▸ ")[0]].plain();
    assert!(
        run.ends_with(crate::tui::line::spark_glyph(Some(40))),
        "the `ral` call keeps the magnitude it earned: {run:?}"
    );
}

/// The printer never mints a block itself: the mirror only ever draws what
/// the fold has reported.
#[test]
fn the_mirror_draws_what_the_fold_reports() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let _ = log.land(
        &mut sb,
        Record::Display(Display::Prompt {
            text: "hello".into(),
            turn: Some(1),
        }),
    );
    log.say(&mut sb, "hi back");
    let all = text(&mut sb);
    assert!(all.contains("hello") && all.contains("hi back"), "{all:?}");
}

/// Deliberation and the work it ordered read as one group — one `∴` head
/// over one `▸` run — and prose closes it, so the answer after opens its
/// own block.
#[test]
fn deliberation_and_its_work_read_as_one_group() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let _ = log.land(
        &mut sb,
        Record::Display(Display::Thinking {
            text: "weighing the shape\n".into(),
        }),
    );
    let _ = log.call(&mut sb, "read the renderer", "read 'line.rs'");
    let _ = log.land(&mut sb, Record::Display(Display::Turn { id: 1 }));
    let _ = log.call(&mut sb, "read the block", "read 'block.rs'");
    assert_eq!(sb.blocks.len(), 1, "one group holds the whole burst");

    log.say(&mut sb, "and so\n");
    assert_eq!(sb.blocks.len(), 2, "prose closes the group");
    let w = sb.render_window(READ_W, 60);
    let (thinking, run) = (rail_rows(&w.lines, "∴ "), rail_rows(&w.lines, "▸ "));
    assert_eq!(thinking.len(), 1, "one deliberation head");
    assert_eq!(run.len(), 1, "over one run of work");
    assert!(thinking[0] < run[0], "the deliberation is hoisted above it");
}

/// Two dials on one group: the parts move apart, and each row answers to
/// the part it belongs to.
#[test]
fn a_group_carries_a_dial_per_part() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let _ = log.land(
        &mut sb,
        Record::Display(Display::Thinking {
            text: "weighing the shape\n".into(),
        }),
    );
    let _ = log.call(&mut sb, "read the renderer", "read 'line.rs'");
    let _ = sb.render_window(READ_W, 60);

    let hit = |part| Hit { block: 0, part };
    assert!(sb.block_dialable(hit(Part::Thinking)));
    assert!(sb.block_dialable(hit(Part::Run)));
    let head = |part| sb.block_head(hit(part)).expect("both parts are on screen");
    assert!(
        head(Part::Thinking) < head(Part::Run),
        "the two heads are distinct rows"
    );
    assert_eq!(
        sb.block_at(head(Part::Run)).map(|h| h.part),
        Some(Part::Run),
        "a row in the run answers to the run's dial"
    );
    assert!(sb.cycle_block(hit(Part::Run)), "the run takes the dial");
}

/// `/thinking`'s two obligations: the groups already drawn move, and a
/// group that arrives afterwards is born at the rung then in force.
#[test]
fn the_standing_thinking_rung_reaches_past_and_future_groups() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let think = |log: &mut Stream, sb: &mut Scrollback, text: &str| {
        let _ = log.land(sb, Record::Display(Display::Thinking { text: text.into() }));
    };
    think(&mut log, &mut sb, "weighing the shape\n");
    sb.set_thinking_level(Detail::Summary);
    log.say(&mut sb, "the answer\n");
    think(&mut log, &mut sb, "and again\n");

    let w = sb.render_window(READ_W, 60);
    let all = w
        .lines
        .iter()
        .map(Row::plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !all.contains("weighing the shape") && !all.contains("and again"),
        "both deliberations read as their headers alone: {all:?}"
    );
    sb.set_thinking_level(Detail::Full);
    let w = sb.render_window(READ_W, 60);
    let all = w
        .lines
        .iter()
        .map(Row::plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("weighing the shape") && all.contains("and again"),
        "and both open again together: {all:?}"
    );
}

/// Chrome drawn between two commits keeps its place: it is appended where
/// it was drawn, and no later record moves it.
#[test]
fn chrome_holds_its_place_among_the_commits() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let _ = log.land(
        &mut sb,
        Record::Display(Display::Prompt {
            text: "one".into(),
            turn: Some(1),
        }),
    );
    sb.push_chrome(Chrome::Note("between".into()));
    let _ = log.land(
        &mut sb,
        Record::Display(Display::Prompt {
            text: "two".into(),
            turn: Some(3),
        }),
    );

    let rendered = sb
        .render_window(READ_W, 60)
        .lines
        .iter()
        .map(Row::plain)
        .collect::<Vec<_>>();
    let at = |needle: &str| {
        rendered
            .iter()
            .position(|l| l.contains(needle))
            .expect("drawn")
    };
    assert!(
        at("one") < at("between") && at("between") < at("two"),
        "chrome drawn between two commits renders between them: {rendered:?}"
    );
}

// ── the transcript ──────────────────────────────────────────────────────

/// Lines of `sb`'s `user.log` ending in `marker`, which is how a test asks
/// whether a block reached the transcript, and how many times.
fn logged(path: &Path, marker: &str) -> usize {
    fs::read_to_string(path)
        .expect("the transcript reads back")
        .lines()
        .filter(|l| l.ends_with(marker))
        .count()
}

/// The mirror is bounded; the transcript is not.  What the fold's window
/// drops is on disk by the time it goes, so a session outliving that
/// window still leaves a whole `user.log` behind.
#[test]
fn the_transcript_keeps_what_the_window_drops() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    let total = record::BLOCKS_WINDOW + 50;
    for i in 0..total {
        let _ = log.land(
            &mut sb,
            Record::Forensic(crate::record::Forensic::SystemNote {
                text: format!("marker {i}"),
            }),
        );
    }
    assert_eq!(
        sb.blocks.len(),
        record::BLOCKS_WINDOW,
        "the mirror is bounded by the fold's own window"
    );
    let path = sb
        .flush_log()
        .expect("the transcript flushes")
        .to_path_buf();
    let text = fs::read_to_string(&path).expect("the transcript reads back");
    let last = format!("marker {}", total - 1);
    assert_eq!(logged(&path, "marker 0"), 1, "the dropped head is on disk");
    assert_eq!(logged(&path, &last), 1, "and so is the resident tail");
    assert!(
        text.find("marker 0") < text.find(&last),
        "in the order they were written"
    );
}

/// A flush writes the resident mirror provisionally, so `/export`
/// mid-session reads a whole transcript; the next one rewinds over it
/// rather than writing the same blocks twice.
#[test]
fn a_second_flush_rewinds_the_provisional_tail() {
    let mut sb = scrollback();
    for i in 0..3 {
        sb.push_chrome(Chrome::Note(format!("marker {i}")));
    }
    let path = sb
        .flush_log()
        .expect("the transcript flushes")
        .to_path_buf();
    let once = fs::read_to_string(&path).expect("the transcript reads back");

    sb.push_chrome(Chrome::Note("marker 3".into()));
    sb.flush_log().expect("the transcript flushes");
    let twice = fs::read_to_string(&path).expect("the transcript reads back");

    assert!(twice.starts_with(&once), "the first flush is a prefix");
    assert_eq!(logged(&path, "marker 0"), 1, "written once, not twice");
    assert_eq!(logged(&path, "marker 3"), 1, "and the new block joins it");
}

/// A resumed session's mirror was rendered by the run that recorded it, so
/// the continuation appends to that transcript instead of repeating it.
#[test]
fn a_resumed_transcript_continues_rather_than_repeats() {
    let mut first = scrollback();
    let mut log = Stream::new();
    let path = first.log_path.clone();
    let _ = log.land(
        &mut first,
        Record::Display(Display::Prompt {
            text: "one".into(),
            turn: Some(1),
        }),
    );
    first.flush_log().expect("the transcript flushes");
    assert_eq!(logged(&path, "one"), 1);

    let replayed = std::mem::take(&mut first.fold);
    let mut resumed = Scrollback::new(path.clone(), AgentSlot(0), true, Detail::Full);
    resumed.seed(replayed);
    let _ = log.land(
        &mut resumed,
        Record::Display(Display::Prompt {
            text: "two".into(),
            turn: Some(3),
        }),
    );
    resumed.flush_log().expect("the transcript flushes");

    assert_eq!(
        logged(&path, "one"),
        1,
        "the seeded prefix is not rewritten"
    );
    assert_eq!(logged(&path, "two"), 1, "and the new block joins it");
}
