use super::*;
use crate::card::{Card, Mark};
use crate::record::{
    BlockId, Display, Forensic, Locus, Protocol, Record, Recorded, Seq, Verdict, Voice,
};

fn scrollback() -> Scrollback {
    Scrollback::new(AgentSlot(0), Detail::Full, Recorder::none(), None)
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
        sb.step(&Recorded::new(Locus::placeholder(seq), record));
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

/// The open line reads inside the block it will join, and the record that
/// completes it changes the text without changing the picture — the one
/// claim that says live and committed are the same rendering.
#[test]
fn the_record_that_completes_a_line_does_not_change_the_picture() {
    let mut sb = scrollback();
    let mut log = Stream::new();

    // A fence opens and a line of code streams: the fence is recorded, the
    // line is still open.
    sb.push_token("```ral\nlet x = 1");
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
    sb.push_token("\n");
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
    sb.push_thinking("considering the shape");

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

    sb.push_token("First words");
    let all = text(&mut sb);
    assert!(
        !all.contains("considering the shape") && all.contains("First words"),
        "prose closed the run, whose tail the worker recorded at the same delta: {all:?}"
    );

    sb.close_lanes();
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

/// Past the fold's window the oldest blocks retire: `n` records past a full
/// window, the first `n` blocks are gone.
fn fill(sb: &mut Scrollback, log: &mut Stream, n: usize) {
    for i in 0..n {
        let _ = log.land(
            sb,
            Record::Forensic(Forensic::SystemNote {
                text: format!("marker {i}"),
            }),
        );
    }
}

/// Rows and blocks count from the mirror's origin, so a click held across a
/// head trim still names the block it landed on.
#[test]
fn a_hit_survives_the_head_being_trimmed() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    fill(&mut sb, &mut log, record::BLOCKS_WINDOW);
    let _ = sb.render_window(READ_W, 10);
    let row = sb.rows().count() / 2;
    let hit = sb.block_at(row).expect("the row is on screen");
    let head = sb.block_head(hit);
    fill(&mut sb, &mut log, 3);
    let _ = sb.render_window(READ_W, 10);
    assert!(
        sb.block_at(row) == Some(hit),
        "the same row names the same block"
    );
    assert_eq!(sb.block_head(hit), head, "and the block keeps its head row");
    assert!(sb.block_at(0).is_none(), "a retired row names nothing");
    assert!(
        sb.block_head(Hit { block: 0, ..hit }).is_none(),
        "nor does a retired block"
    );
}

/// A scrolled-up viewport holds its place when rows retire above it.
#[test]
fn an_unstuck_viewport_keeps_its_absolute_offset_across_a_head_trim() {
    let mut sb = scrollback();
    let mut log = Stream::new();
    fill(&mut sb, &mut log, record::BLOCKS_WINDOW);
    let _ = sb.render_window(READ_W, 10);
    sb.scroll_to(12);
    assert_eq!(sb.render_window(READ_W, 10).offset, 12);
    fill(&mut sb, &mut log, 2);
    let w = sb.render_window(READ_W, 10);
    assert_eq!(w.offset, 12, "the window stays on the same rows");
    assert!(!sb.sticky);
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
    sb.push_thinking("considering the shape");
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
        Record::Display(Display::Verdict {
            call: BlockId::new(call),
            verdict: Verdict::of(&"a line\n".repeat(40), false),
        }),
    );

    let w = sb.render_window(READ_W, 40);
    let act = rail_rows(&w.lines, "↗ ");
    assert_eq!(w.lines[act[0]].plain(), "cancel     [hunter]");
    let run = w.lines[rail_rows(&w.lines, "▸ ")[0]].plain();
    assert!(
        run.ends_with(crate::tui::gauge::spark_glyph(Some(40))),
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
        Record::Protocol(Protocol::UserPrompt {
            turn: 1,
            text: "hello".into(),
            voice: Voice::Human,
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
    let _ = log.land(
        &mut sb,
        Record::Forensic(Forensic::TurnStarted {
            turn: 1,
            tuning: crate::provider::Tuning::initial(),
            route: None,
        }),
    );
    let _ = log.call(&mut sb, "read the block", "read 'block.rs'");
    assert_eq!(sb.mirror.blocks.len(), 1, "one group holds the whole burst");

    log.say(&mut sb, "and so\n");
    assert_eq!(sb.mirror.blocks.len(), 2, "prose closes the group");
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
        Record::Protocol(Protocol::UserPrompt {
            turn: 1,
            text: "one".into(),
            voice: Voice::Human,
        }),
    );
    sb.push_chrome(Chrome::Note("between".into()));
    let _ = log.land(
        &mut sb,
        Record::Protocol(Protocol::UserPrompt {
            turn: 3,
            text: "two".into(),
            voice: Voice::Human,
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

// ── the log ─────────────────────────────────────────────────────────────

fn prompt(text: &str) -> Protocol {
    Protocol::UserPrompt {
        turn: 1,
        text: text.into(),
        voice: Voice::Human,
    }
}

/// A record log publishing onto a fresh channel, and the facts it has sent.
fn on_air(recorder: &Recorder) -> crate::bus::BusReceiver {
    let (tx, rx) = crate::bus::channel();
    recorder.attach(crate::bus::Emitter::new(tx, crate::record::AgentId::new(0)).fleet_sink());
    rx
}

fn heard(rx: &crate::bus::BusReceiver) -> Vec<Recorded<Record>> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|sig| match sig {
            crate::bus::Signal::Fact(_, rec) => Some(rec),
            crate::bus::Signal::Born(..) | crate::bus::Signal::Transient(..) => None,
        })
        .collect()
}

/// A mirror born after its log began reads what it missed, folds what it
/// then hears again only once, and reads past a gap rather than skip it.
#[test]
fn a_late_mirror_catches_up_hears_nothing_twice_and_reads_past_a_gap() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let recorder = Recorder::create(&dir.path().join("record.jsonl")).expect("a record log");
    let emit = |text| {
        let _recorded = recorder.emit(prompt(text)).expect("the log appends");
    };
    emit("alpha");
    let rx = on_air(&recorder);
    emit("bravo");
    let mut sb = Scrollback::new(AgentSlot(0), Detail::Full, recorder.clone(), None);
    sb.catch_up();
    for rec in heard(&rx) {
        sb.fact(&rec);
    }
    emit("charlie");
    emit("delta");
    let delta = heard(&rx).pop().expect("delta is heard");
    sb.fact(&delta);

    let all = text(&mut sb);
    for word in ["alpha", "bravo", "charlie", "delta"] {
        assert_eq!(all.matches(word).count(), 1, "{word} once: {all:?}");
    }
}

/// A `/clear` is a record like any other: the mirror drops what it held,
/// draws the opening again, and reads on from where it was.
#[test]
fn a_cleared_record_starts_the_mirror_over_and_keeps_the_cursor() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let recorder = Recorder::create(&dir.path().join("record.jsonl")).expect("a record log");
    let rx = on_air(&recorder);
    let _recorded = recorder.emit(prompt("before")).expect("the log appends");
    let opening = Card(vec![Mark::heading("the session card")]);
    let mut sb = Scrollback::new(AgentSlot(0), Detail::Full, recorder.clone(), Some(opening));
    sb.catch_up();
    assert!(text(&mut sb).contains("before"));
    let _recorded = recorder.emit(Protocol::Cleared).expect("the log appends");
    let _recorded = recorder.emit(prompt("after")).expect("the log appends");
    let mut live = heard(&rx);
    let after = live.pop().expect("the prompt after the clear");
    let cleared = live.pop().expect("the clear");

    assert!(sb.fact(&cleared), "a clear starts the mirror over");
    assert!(!sb.fact(&after), "a prompt does not");

    let all = text(&mut sb);
    assert!(all.contains("the session card"), "{all:?}");
    assert!(all.contains("[cleared]"), "{all:?}");
    assert!(all.contains("after"), "{all:?}");
    assert!(!all.contains("before"), "{all:?}");
    assert_eq!(
        sb.cursor.map(Cursor::next),
        Some(after.locus().seq().get() + 1),
        "the cursor stayed on the log"
    );
}
