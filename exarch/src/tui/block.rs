//! The reader's atom.
//!
//! A [`Block`] is what a reader takes in at once, and a session's scrollback
//! is a `Vec<Block>` mirroring the view fold one incident at a time.  Two
//! kinds of block carry a [`Detail`] dial: a [`Group`] — one burst of
//! deliberation and the work it ordered — carries one per part, and an act or
//! a diff card carries the one.  Prose is product to read rather than process
//! to reduce, and chrome is already a line or two, so both render whole.
//!
//! A block memoises the visual rows it last produced, keyed by the width they
//! were built at, so a dial re-renders one block and a resize the mirror.

use super::banner;
use super::fidelity::Fidelity;
use super::group;
use super::line::{self, is_blank};
use super::md::{self, MD_INDENT};
use super::palette::{QUEUED_PROMPT_BG, READ_W, SLATE, content_w};
use super::rail::{self, RailKind};
use super::row::Row;
use crate::agent::event::ProviderErrorRecord;
use crate::bus::card::{Card, Landing, Mark, Span as CardSpan};
use crate::record::Seq;
use ral_core::types::Observed;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;

/// Index into [`super::palette::AGENT_HUES`], wrapping: root is `0`, each
/// subagent the next slot at birth.  Carried by value, so the rail needs no
/// lookup on `App`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct AgentSlot(pub u8);

/// Chrome as data: every builder renders itself fresh at the width its block
/// is shown at, rather than laying out once against a fixed budget the mirror
/// then wraps a second time.
pub(super) enum Chrome {
    Turn,
    /// The human's turn, tinted [`super::palette::PROMPT_INK`] and ruled
    /// full-width by [`seat_rows`].  No band — background is the machine's.
    Prompt(String),
    /// A meta-notice — a model switch, an export, a stall: an annotation
    /// rather than a navigable block.
    Note(String),
    StopReason(String),
    Error(String),
    ProviderError(ProviderErrorRecord),
    Stalled(ProviderErrorRecord),
    /// The turn the human stopped: it wears the `╳` an error does — the work
    /// broke off either way — but stays a separate shape so the matrix's
    /// failure cell keeps reporting failures only.
    Cancelled,
    /// A detached block that settled — the `` `done `` a deferred worker flushes
    /// at completion. It wears the `↘` of [`RailKind::Subagent`]: background
    /// work landing in root's scrollback turns after the run that spawned it is
    /// the same event as an agent's answer arriving, whatever produced it.
    Settled(Vec<CardSpan>),
    /// A detached worker's birth. It wears the `↗` of [`RailKind::FleetAct`],
    /// the act that made it: work leaving the run, to which [`Self::Settled`]
    /// is the `↘` of its return.
    Spawned(Vec<CardSpan>),
    /// The startup wordmark over the session card, one picture.
    Opening(Card),
    Legend,
    /// A general surfaced card framed at the block's own width — the
    /// `/resources` readout.
    Framed(Card),
}

impl Chrome {
    /// Lay this chrome out at `width`, the content measure of its block.  Free
    /// text wraps here; the field-row and card builders take the width
    /// themselves.
    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let text = |lines: Vec<Line<'static>>| -> Vec<Line<'static>> {
            lines
                .iter()
                .flat_map(|l| line::wrap_line(l, width.into()))
                .collect()
        };
        match self {
            Self::Turn => line::turn(),
            Self::Prompt(s) => text(line::user_prompt(s)),
            Self::Note(s) => text(line::note(s)),
            Self::StopReason(raw) => text(line::stop_reason(raw)),
            Self::Error(msg) => text(line::error(msg)),
            Self::ProviderError(e) => line::provider_error(e, width),
            Self::Stalled(e) => line::stalled(e, width),
            Self::Cancelled => line::note("cancelled"),
            Self::Settled(spans) | Self::Spawned(spans) => text(line::render_text(spans)),
            Self::Opening(card) => banner::opening(card, width),
            Self::Legend => text(banner::legend_panel(width)),
            Self::Framed(card) => {
                line::render_card_framed(card, line::CARD_INDENT, width, Detail::Full)
            }
        }
    }

    fn rail(&self) -> Option<RailKind> {
        match self {
            Self::Turn => Some(RailKind::Turn),
            Self::Settled(_) => Some(RailKind::Subagent),
            Self::Spawned(_) => Some(RailKind::FleetAct),
            Self::Error(_) | Self::ProviderError(_) | Self::Stalled(_) | Self::Cancelled => {
                Some(RailKind::Error)
            }
            Self::Note(_) | Self::StopReason(_) | Self::Legend | Self::Framed(_) => {
                Some(RailKind::Note)
            }
            Self::Opening(_) => None,
            Self::Prompt(_) => Some(RailKind::Prompt),
        }
    }
}

/// How much of a dialable part is disclosed, low to high; `Ord` compares the
/// rungs.  A run of `ral` work and a diff reach [`Self::Tally`]; a
/// deliberation and an act floor at [`Self::Summary`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Detail {
    /// The numbers alone: a run's `|>` effects counted on one line, a diff's
    /// header with its size bar and grain.
    Tally,
    /// The representative slice: the run's live tip, a deliberation's header,
    /// an act's row, a diff's first [`super::line::DIFF_PEEK_ROWS`] rows.
    Summary,
    /// The whole thing: every call with its source, the whole deliberation,
    /// the whole payload, every hunk.
    Full,
}

impl Detail {
    /// One rung up, the ceiling wrapping to `floor`, so a dial walks every
    /// reachable rung rather than toggling the extremes.
    fn next(self, floor: Self) -> Self {
        match self {
            Self::Tally => Self::Summary,
            Self::Summary => Self::Full,
            Self::Full => floor,
        }
    }
}

/// One of a [`Group`]'s two dialable parts — the `∴` deliberation, or the `▸`
/// run of work.  Every other block has the one dial, which answers to
/// [`Self::Run`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) enum Part {
    Thinking,
    #[default]
    Run,
}

/// A group's `∴` part: each stretch of thinking the fold committed, in
/// arrival order; the mass of the prose they became — the deliberation
/// grain's denominator, which the commit cannot carry, being recorded before
/// the prose it precedes — and the rung the part is read at.
pub(super) struct Thinking {
    text: Vec<String>,
    answer_chars: u32,
    at: Detail,
}

impl Thinking {
    fn new(at: Detail) -> Self {
        Self {
            text: Vec::new(),
            answer_chars: 0,
            at,
        }
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The deliberation's mass — the grain's numerator.
    fn chars(&self) -> u32 {
        let n: usize = self.text.iter().map(|t| t.chars().count()).sum();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Its bulk — the header's size bar and the rail's value step.  Both
    /// saturate: a count read as a magnitude may not wrap.
    fn lines(&self) -> u32 {
        let n: usize = self.text.iter().map(|t| t.lines().count()).sum();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// The grain and bulk of the whole, and — past the header rung — each
    /// stretch in turn.  `open` is the line no record covers yet, joined onto
    /// the last stretch.
    fn body(&self, at: Detail, width: u16, open: &str) -> Vec<Line<'static>> {
        let mut ls = line::thinking_header(self.chars(), self.lines(), self.answer_chars);
        if at >= Detail::Full {
            // One deliberation, one document: the stretches are joined as
            // paragraphs, so the seam between two of them reads as a break
            // and not as a wrap.
            let mut text = self
                .text
                .iter()
                .map(|t| t.trim_end())
                .collect::<Vec<_>>()
                .join("\n\n");
            if !open.is_empty() {
                // The line the last stretch is still speaking, on its own row:
                // the newline the stretch was trimmed of is the one that
                // separates them.
                text.push('\n');
                text.push_str(open);
            }
            ls.push(Line::default());
            ls.extend(md::render_thinking(&text, width, MD_INDENT));
        }
        ls
    }
}

/// One burst of deliberation and the work it ordered, read as a single
/// object: the thinking hoisted above the calls it led to, so a turn reads
/// *thought, work, answer* rather than in the interleaving the wire happened
/// to deliver.  Deliberation alone holds no calls; a lone call no thinking.
pub(super) struct Group {
    thinking: Thinking,
    calls: Vec<group::Call>,
    /// The `▸` part's own rung, dialled apart from the `∴` part's.
    run: Detail,
    /// Which part the last member arrived on — what an open line grows.
    last: Part,
}

/// What may join a group: deliberation, work, the effects that work
/// produced, and the turn rules between them.
pub(super) enum Member {
    Thinking(String),
    Call(group::Call),
    /// One of a call's `|>` effects, as the fact itself: the group dedupes it
    /// and renders its bucket at every width.
    Effect(Observed),
    /// A turn boundary interior to a burst: each call is its own provider
    /// round-trip, so one lands between consecutive calls.  Bookkeeping,
    /// drawn as nothing — left a barrier it would cut every burst to one call.
    Turn,
}

impl Group {
    /// Open a group on `member`, its deliberation born at the standing rung.
    fn opening(member: Member, thinking: Detail) -> Self {
        let mut group = Self {
            thinking: Thinking::new(thinking),
            calls: Vec::new(),
            run: Detail::Summary,
            last: Part::Run,
        };
        let _ = group.grow(member);
        group
    }

    /// Whether `member` may join this group.  An effect belongs to the call
    /// that issued it, so one reaching a group with no call belongs to none.
    fn admits(&self, member: &Member) -> bool {
        !matches!(member, Member::Effect(_)) || !self.calls.is_empty()
    }

    /// Take `member` in, reporting whether the group's picture moved: a turn
    /// rule interior to a burst is bookkeeping and changes nothing.
    fn grow(&mut self, member: Member) -> bool {
        match member {
            Member::Thinking(text) => {
                self.thinking.text.push(text);
                self.last = Part::Thinking;
            }
            Member::Call(call) => {
                self.calls.push(call);
                self.last = Part::Run;
            }
            Member::Effect(what) => {
                let Some(call) = self.calls.last_mut() else {
                    return false;
                };
                call.absorb(what);
            }
            Member::Turn => return false,
        }
        true
    }
}

/// What a block carries — each variant a pure function of its data, the
/// target width, and its own rung.
pub(super) enum BlockKind {
    /// One fold Answer block.  `continues` is true when the previous
    /// rendering block is prose too: this paragraph then keeps the margin and
    /// drops the `·`, so one response wears one rail mark.
    Prose {
        src: String,
        fidelity: Fidelity,
        continues: bool,
    },
    Group(Group),
    /// A harness act — `spawn`, `cancel`, `message`, `reply`, `schedule`,
    /// `unschedule`.  It changes the world outside the turn, so it is no
    /// observation: never folded into a run, and carrying no magnitude.
    Act {
        verb: String,
        subject: Option<String>,
        payload: String,
        failed: bool,
        at: Detail,
    },
    /// An async subagent's result, landed in root's scrollback.  Its own kind
    /// because prose cannot carry `name`/`elapsed`/`error` and a card would
    /// lose the `↘` identity.
    Subagent {
        name: String,
        error: Option<String>,
        elapsed: Duration,
    },
    /// A render document a kit surfaced — a stack of [`Card`] marks
    /// re-rendered from data at every width.  Only one holding a `diff` mark
    /// is dialable.
    Card {
        card: Card,
        landing: Landing,
        at: Detail,
    },
    /// A summary-less tool call, inert under the shut triangle.  `details` is
    /// `None` for a parse failure (`INVALID_INPUT`): such a call renders
    /// nothing, present only as the barrier a stray result stops at.
    Tool {
        details: Option<String>,
    },
    /// Meta content the mirror draws itself, not authored by a fold block.
    Chrome(Chrome),
}

impl BlockKind {
    /// A card at its opening rung: a diff arrives as its first rows, so the
    /// change is read before it is measured; every other card is inert and
    /// renders whole.
    pub(super) fn card(card: Card, landing: Landing) -> Self {
        let at = if card.has_diff() {
            Detail::Summary
        } else {
            Detail::Full
        };
        Self::Card { card, landing, at }
    }
}

/// Rows for what the human has typed and is still waiting on — prompts and the
/// commands queued among them — in the very chrome a committed prompt wears,
/// washed to read as pending and capped to `max_rows`.
///
/// Stripped to the ink, unlike the transcript: a chrome block frames itself in
/// blank rows so it can breathe among its neighbours, and the strip has none —
/// it is a stack of pending turns, each already fenced off from the last, in a
/// band that grows downward into the reading the user is waiting on.
pub(super) fn queued_prompt_rows(messages: &[String], width: u16, max_rows: usize) -> Vec<Row> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for message in messages {
        let prompt = Block::chrome(Chrome::Prompt(message.clone()), None);
        let (seated, _) = prompt.seated(width, AgentSlot::default(), None, "");
        let rows = trim_blanks(seated, Row::is_blank);
        let _ = seat_rows(&mut out, rows, width, true, Some(QUEUED_PROMPT_BG));
    }

    if out.len() > max_rows {
        let hidden = out.len() - (max_rows - 1);
        out.truncate(max_rows - 1);
        out.push(
            Row::bare(Line::from(Span::styled(
                format!("⋯ ({hidden} more)"),
                Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
            )))
            .wash(QUEUED_PROMPT_BG, width),
        );
    }
    out
}

/// Seat block-rendered rows onto the screen — the shared last step of the
/// transcript and the queued-prompt projection.  Every body already rendered
/// at `width`, so this only fences and washes; it no longer wraps.  With
/// `prompt` set the fence goes in above the first visible row and outside any
/// `wash`: a boundary marks the plane's edge rather than lying within it, so a
/// prompt's rule reads the same committed or queued.
pub(super) fn seat_rows(
    out: &mut Vec<Row>,
    rows: Vec<Row>,
    width: u16,
    prompt: bool,
    wash: Option<Color>,
) -> usize {
    let before = out.len();
    let mut fenced = false;
    for row in rows {
        if prompt && !fenced && !row.is_blank() {
            out.push(line::prompt_fence(width));
            fenced = true;
        }
        out.push(match wash {
            Some(bg) => row.wash(bg, width),
            None => row,
        });
    }
    out.len() - before
}

/// `rows` after a tail that is or is not blank: a block's leading blanks
/// collapse against an already-blank tail, so a turn separator before
/// leading-blank chrome reads as one gap.  The transcript's one seam rule,
/// read by the screen, by a group's two parts, and by `user.log` alike.
pub(super) fn seam(prev_blank: bool, rows: &[Row]) -> &[Row] {
    if !prev_blank {
        return rows;
    }
    &rows[rows.iter().take_while(|r| r.is_blank()).count()..]
}

/// A block's rendering at one width, and where its two parts divide.
struct Memo {
    width: u16,
    rows: Vec<Row>,
    /// Visual rows the `∴` part holds; the rest are the `▸` part's.  `0` for
    /// every block that has only the one part.
    thinking: usize,
}

/// A block paired with the visual rows it last rendered.
pub(super) struct Block {
    kind: BlockKind,
    /// The fold block this was built from — `None` for chrome, which no
    /// record authors.  It is this block's place in the fold's order, so the
    /// mirror's head trim and a result patch both address it by this.
    seq: Option<Seq>,
    memo: Option<Memo>,
}

impl Block {
    pub(super) fn new(kind: BlockKind, seq: Option<Seq>) -> Self {
        Self {
            kind,
            seq,
            memo: None,
        }
    }

    pub(super) fn prose(
        src: String,
        fidelity: Fidelity,
        continues: bool,
        seq: Option<Seq>,
    ) -> Self {
        Self::new(
            BlockKind::Prose {
                src,
                fidelity,
                continues,
            },
            seq,
        )
    }

    /// A group opening on `member`, its deliberation born at `thinking`.
    pub(super) fn group(member: Member, thinking: Detail, seq: Option<Seq>) -> Self {
        Self::new(BlockKind::Group(Group::opening(member, thinking)), seq)
    }

    pub(super) fn chrome(chrome: Chrome, seq: Option<Seq>) -> Self {
        Self::new(BlockKind::Chrome(chrome), seq)
    }

    /// Where this block sits in the fold's order — `None` for chrome, which
    /// inherits the expiry of the block it was drawn after.
    pub(super) fn seq(&self) -> Option<Seq> {
        self.seq
    }

    fn group_mut(&mut self) -> Option<&mut Group> {
        match &mut self.kind {
            BlockKind::Group(group) => Some(group),
            _ => None,
        }
    }

    /// Take `member` into this block's group, or hand it back for the next
    /// block to try.  A group standing at the mirror's tail is open by
    /// construction: every barrier pushes a block after it, so a group that is
    /// still the tail is a group nothing has closed.
    pub(super) fn admit(&mut self, member: Member) -> Option<Member> {
        let Some(group) = self.group_mut() else {
            return Some(member);
        };
        if !group.admits(&member) {
            return Some(member);
        }
        if group.grow(member) {
            self.memo = None;
        }
        None
    }

    /// Grow this block's lone diff with `card`'s hunks where both are surfaced
    /// diffs of one file, so consecutive edits to it read as the one change;
    /// `Some` hands back a card that does not fit.  The hunks carry the
    /// magnitude, so the memo goes with them.
    pub(super) fn merge_diff(&mut self, card: Card) -> Option<Card> {
        let BlockKind::Card {
            card: tail,
            landing: Landing::Surfaced,
            ..
        } = &mut self.kind
        else {
            return Some(card);
        };
        match (tail.single_diff(), card.single_diff()) {
            (Some((into, _)), Some((path, _))) if into == path => {}
            _ => return Some(card),
        }
        let Card(mut marks) = card;
        match (marks.pop(), tail.0.as_mut_slice()) {
            (Some(Mark::Diff { hunks, .. }), [Mark::Diff { hunks: into, .. }]) => {
                into.extend(hunks);
            }
            _ => unreachable!("both cards answered `single_diff`"),
        }
        self.memo = None;
        None
    }

    /// The human turn's echo — the one block ruled full-width.
    fn prompt(&self) -> bool {
        matches!(self.kind, BlockKind::Chrome(Chrome::Prompt(_)))
    }

    /// Whether this block puts anything on screen.  A call whose input did
    /// not parse renders nothing, and a prose run reads across it.
    pub(super) fn renders(&self) -> bool {
        !matches!(self.kind, BlockKind::Tool { details: None })
    }

    pub(super) fn is_prose(&self) -> bool {
        matches!(self.kind, BlockKind::Prose { .. })
    }

    /// The session's *code* footprint — a card's changed lines and nothing
    /// else.  The matrix's "lines touched" is a write footprint, not a volume.
    pub(super) fn lines_changed(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Card { card, .. } => card.magnitude(),
            _ => None,
        }
    }

    /// The epistemic signal this block was built with — what a live tail
    /// re-renders under, so growing prose keeps the ink it commits in.  Sound
    /// (`0/0`) off the prose lane: only prose degrades its medium.
    pub(super) fn fidelity(&self) -> Fidelity {
        match &self.kind {
            BlockKind::Prose { fidelity, .. } => *fidelity,
            _ => Fidelity::default(),
        }
    }

    /// Whether an open prose line continues this block.
    pub(super) fn open_prose(&self) -> bool {
        self.is_prose()
    }

    /// The mass of the prose this block holds — the deliberation grain's
    /// denominator, for the group above it.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "prose char count; u32 headroom far exceeds any one answer"
    )]
    pub(super) fn prose_chars(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Prose { src, .. } => Some(src.chars().count() as u32),
            _ => None,
        }
    }

    /// Whether an open thinking line continues this block: a group renders
    /// its deliberation above its work, so only one whose last member was a
    /// stretch of thinking is still speaking.
    pub(super) fn open_thinking(&self) -> bool {
        matches!(&self.kind, BlockKind::Group(g) if g.last == Part::Thinking)
    }

    /// Replace the text the fold just grew — the prose this block is, or its
    /// group's last stretch of thinking — with the memo's own.
    pub(super) fn set_text(&mut self, text: String) {
        match &mut self.kind {
            BlockKind::Prose { src, .. } => *src = text,
            BlockKind::Group(g) => match g.thinking.text.last_mut() {
                Some(last) => *last = text,
                None => return,
            },
            _ => return,
        }
        self.memo = None;
    }

    /// Restamp a prose block's epistemic signal, which its growing text and
    /// the turn's own pressure both move.
    pub(super) fn set_fidelity(&mut self, ink: Fidelity) {
        if let BlockKind::Prose { fidelity, .. } = &mut self.kind
            && *fidelity != ink
        {
            *fidelity = ink;
            self.memo = None;
        }
    }

    /// The mass of the prose this block's deliberation became — the grain's
    /// denominator, which the view measures because the commit precedes it.
    pub(super) fn set_answer_chars(&mut self, chars: u32) {
        if let BlockKind::Group(g) = &mut self.kind
            && g.thinking.answer_chars != chars
        {
            g.thinking.answer_chars = chars;
            self.memo = None;
        }
    }

    /// Stamp the result magnitude on the call `at` names, if this block holds
    /// it.  Reports whether it did, so the mirror's walk stops there.
    pub(super) fn measure(&mut self, at: Seq, n: u32) -> bool {
        let Some(call) = self
            .group_mut()
            .and_then(|g| g.calls.iter_mut().find(|c| c.at() == at))
        else {
            return false;
        };
        call.measure(n);
        self.memo = None;
        true
    }

    /// The lowest rung `part` of this block reduces to — `None` where the
    /// part has no dial.  A run and a diff have numbers to reduce to; a
    /// deliberation and an act have only their header.
    fn floor(&self, part: Part) -> Option<Detail> {
        match (&self.kind, part) {
            (BlockKind::Group(g), Part::Thinking) => {
                (!g.thinking.is_empty()).then_some(Detail::Summary)
            }
            (BlockKind::Group(g), Part::Run) => (!g.calls.is_empty()).then_some(Detail::Tally),
            (BlockKind::Act { .. }, Part::Run) => Some(Detail::Summary),
            (BlockKind::Card { card, .. }, Part::Run) => card.has_diff().then_some(Detail::Tally),
            _ => None,
        }
    }

    /// Whether `part` is dialable — a property of its kind, not its rung, so
    /// a click on its glyph claims the gesture even at the ceiling.
    pub(super) fn dialable(&self, part: Part) -> bool {
        self.floor(part).is_some()
    }

    /// One click on `part`: a rung up, wrapping at the ceiling to its floor.
    pub(super) fn dial(&mut self, part: Part) -> bool {
        let Some(floor) = self.floor(part) else {
            return false;
        };
        match (&mut self.kind, part) {
            (BlockKind::Group(g), Part::Thinking) => g.thinking.at = g.thinking.at.next(floor),
            (BlockKind::Group(g), Part::Run) => g.run = g.run.next(floor),
            (BlockKind::Act { at, .. } | BlockKind::Card { at, .. }, _) => *at = at.next(floor),
            _ => return false,
        }
        self.memo = None;
        true
    }

    /// Move this block's deliberation to the standing `/thinking` rung.
    pub(super) fn set_thinking(&mut self, at: Detail) {
        if let BlockKind::Group(g) = &mut self.kind
            && g.thinking.at != at
        {
            g.thinking.at = at;
            self.memo = None;
        }
    }

    /// Fill the render memo at `width`, so one walk of the mirror sees the
    /// whole screen.  A block whose memo already stands renders nothing again.
    pub(super) fn fill(&mut self, width: u16, agent: AgentSlot) {
        if self.memo.as_ref().is_some_and(|m| m.width == width) {
            return;
        }
        self.memo = Some(self.wrapped(width, agent, None, ""));
    }

    /// The rows the last [`Self::fill`] left behind — empty for a block no
    /// render pass has reached yet.  Every row index the frontend hands
    /// around is measured against these.
    pub(super) fn rendered(&self) -> &[Row] {
        self.memo.as_ref().map_or(&[], |m| m.rows.as_slice())
    }

    /// This block's rows with `open` — the lane's still-uncommitted line —
    /// spliced onto the text it will join.  Never memoised: the line moves
    /// with every delta, and the record that completes it must change the
    /// text without changing the picture.
    pub(super) fn live(&self, width: u16, agent: AgentSlot, open: &str) -> Vec<Row> {
        self.wrapped(width, agent, None, open).rows
    }

    /// The block as it belongs in the session log: at the readable width and
    /// forced whole, so the script, diff or prose is on the record even while
    /// reduced on screen.
    pub(super) fn log_rows(&self, agent: AgentSlot) -> Vec<Row> {
        self.wrapped(READ_W, agent, Some(Detail::Full), "").rows
    }

    /// The part visual row `i` of this block belongs to — which dial a click
    /// there moves.
    pub(super) fn part_at(&self, i: usize) -> Part {
        match self.memo.as_ref() {
            Some(m) if i < m.thinking => Part::Thinking,
            _ => Part::Run,
        }
    }

    /// The first visual row of `part`.
    pub(super) fn part_start(&self, part: Part) -> usize {
        match (part, self.memo.as_ref()) {
            (Part::Run, Some(m)) => m.thinking,
            _ => 0,
        }
    }

    fn wrapped(&self, width: u16, agent: AgentSlot, at: Option<Detail>, open: &str) -> Memo {
        let (mut head, split) = self.seated(width, agent, at, open);
        let tail = head.split_off(split);
        let mut rows = Vec::new();
        let thinking = seat_rows(&mut rows, head, width, false, None);
        let _ = seat_rows(&mut rows, tail, width, self.prompt(), None);
        Memo {
            width,
            rows,
            thinking,
        }
    }

    /// Build each part's body in content space and seat its rail glyph on the
    /// part's first content row, returning the rows and where the `∴` part
    /// ends.  `at` overrides every part's own rung; `open` is the lane's
    /// still-uncommitted line.
    fn seated(
        &self,
        width: u16,
        agent: AgentSlot,
        at: Option<Detail>,
        open: &str,
    ) -> (Vec<Row>, usize) {
        let content = content_w(width);
        if let BlockKind::Group(g) = &self.kind {
            let mut rows = Vec::new();
            if !g.thinking.is_empty() {
                let body = g.thinking.body(at.unwrap_or(g.thinking.at), content, open);
                let glyph = rail::span(RailKind::Thinking, agent, Some(g.thinking.lines()));
                rows.extend(Row::seat(body, Some(glyph)));
            }
            let split = rows.len();
            if !g.calls.is_empty() {
                let run = at.unwrap_or(g.run);
                let body = group::body(&g.calls, run, content.into());
                let glyph = rail::span(
                    RailKind::ToolCall(run >= Detail::Full),
                    agent,
                    group::aggregate_magnitude(&g.calls),
                );
                // The two parts meet under the one seam rule, so the run's
                // leading blank never doubles the deliberation's.
                let seated = Row::seat(body, Some(glyph));
                let blank = rows.last().is_some_and(Row::is_blank);
                rows.extend_from_slice(seam(blank, &seated));
            }
            return (rows, split);
        }
        let mut lines = self.body(content, at.unwrap_or(Detail::Full), open);
        // Prose is the one body that opens flush, so a lead answer would abut
        // the work above it; the mirror folds this blank against any trailing
        // one, so the gap never doubles.
        if self.leads() && self.is_prose() && !lines.first().is_some_and(is_blank) {
            lines.insert(0, Line::default());
        }
        let glyph = self
            .rail_kind()
            .filter(|_| self.leads())
            .map(|kind| rail::span(kind, agent, self.magnitude()));
        (Row::seat(lines, glyph), 0)
    }

    /// Whether this block leads its own rail mark.  A continuing paragraph
    /// keeps the margin and drops the glyph, so one response wears one `·`.
    fn leads(&self) -> bool {
        !matches!(
            self.kind,
            BlockKind::Prose {
                continues: true,
                ..
            }
        )
    }

    /// Changed lines for a card, source lines for prose.  The rail's value
    /// step reads it, so prose volume lightens the rail as a diff's does.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "transcript line count; u32 headroom far exceeds any in-memory transcript"
    )]
    fn magnitude(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Prose { src, .. } => Some(src.lines().count() as u32),
            BlockKind::Card { card, .. } => card.magnitude(),
            _ => None,
        }
    }

    /// The rail-less body of a one-part block at `width`, graded by `at`.
    fn body(&self, width: u16, at: Detail, open: &str) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::Prose { src, fidelity, .. } => {
                let text = if open.is_empty() {
                    src.clone()
                } else {
                    format!("{src}{open}")
                };
                md::render_md(&text, width, MD_INDENT, *fidelity)
            }
            // The row *is* the act, so there are only two readings: the
            // payload cut to its column, or laid out whole.
            BlockKind::Act {
                verb,
                subject,
                payload,
                failed,
                ..
            } => line::act_row(
                verb,
                subject.as_deref(),
                payload,
                // `reply` is the one act whose payload is a ral value rather
                // than a sentence, so it is the one that reads in ral's hues.
                &match (*failed, verb.as_str()) {
                    (true, _) => line::Payload::Refusal,
                    (false, "reply") => line::Payload::Value,
                    _ => line::Payload::Prose,
                },
                width,
                at >= Detail::Full,
            ),
            BlockKind::Subagent {
                name,
                error,
                elapsed,
            } => line::subagent_header(name, error.as_deref(), *elapsed),
            // A surfaced general card is a deliberate bounded artifact. Diffs
            // already carry the patch rail and gutters, and an effect card
            // that reached the mirror with no run to join belongs to none, so
            // both render unframed.
            BlockKind::Card { card, landing, .. } => {
                if !card.has_diff() && *landing == Landing::Surfaced {
                    line::render_card_framed(card, line::CARD_INDENT, width, at)
                } else {
                    line::render_card_unframed(card, width.into(), at)
                }
            }
            BlockKind::Tool { details } => match details {
                Some(q) => line::tool_call_static(q, width),
                None => Vec::new(),
            },
            // A notice is not prose: it sits in its own gap, one blank row
            // above and below, whatever blanks its builder happened to bring.
            // The mirror collapses the gap against a blank tail, so framing
            // here reads as one row between neighbours, never two.
            BlockKind::Chrome(chrome) => {
                let body = trim_blanks(chrome.render(width), |l| line::is_blank(l));
                if body.is_empty() {
                    // The turn rule *is* a gap; there is nothing to frame.
                    vec![Line::default()]
                } else {
                    std::iter::once(Line::default())
                        .chain(body)
                        .chain(std::iter::once(Line::default()))
                        .collect()
                }
            }
            // A group renders each of its parts in `seated`; it has no
            // one-part body of its own.
            BlockKind::Group(_) => Vec::new(),
        }
    }

    /// The rail shape a one-part block wears, `None` for one that seats no
    /// rail.  A group's two glyphs are seated per part instead.
    fn rail_kind(&self) -> Option<RailKind> {
        match &self.kind {
            BlockKind::Prose { .. } => Some(RailKind::Markdown),
            // A summary-less query is a tool call still, shut.
            BlockKind::Tool { .. } => Some(RailKind::ToolCall(false)),
            // The shape says when the act lands — `◷` on a clock, `↗` now —
            // and holds across every rung: an act is one thing disclosed.
            BlockKind::Act { verb, .. } => Some(match verb.as_str() {
                "schedule" | "unschedule" => RailKind::TimeAct,
                _ => RailKind::FleetAct,
            }),
            // The `↘` holds even on error; the failure reads in the header.
            BlockKind::Subagent { .. } => Some(RailKind::Subagent),
            // A diff and a write are both file mutations, so both wear `▎` and
            // the body says which. A framed card's frame is its own mark, and
            // an unowned effect folds into nothing, so neither seats a glyph.
            BlockKind::Card { card, landing, .. } => {
                (card.has_diff() || *landing == Landing::Write).then_some(RailKind::Patch)
            }
            BlockKind::Chrome(chrome) => chrome.rail(),
            BlockKind::Group(_) => None,
        }
    }
}

/// `items` without its leading and trailing blank rows.
fn trim_blanks<T>(mut items: Vec<T>, blank: impl Fn(&T) -> bool) -> Vec<T> {
    let tail = items.iter().rev().take_while(|i| blank(i)).count();
    items.truncate(items.len() - tail);
    let head = items.iter().take_while(|i| blank(i)).count();
    items.drain(..head);
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(verb: &str, subject: Option<&str>, payload: &str, failed: bool) -> Block {
        Block::new(
            BlockKind::Act {
                verb: verb.into(),
                subject: subject.map(str::to_string),
                payload: payload.into(),
                failed,
                at: Detail::Summary,
            },
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
            .seated(READ_W, AgentSlot(0), None, "")
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
            "spawn         [hunter] audit every unwrap() in exarch/src"
        );
        assert_eq!(
            rendered("unschedule", Some("nightly"), ""),
            "unschedule    [nightly]",
            "a landed act with no argument leaves the payload cell empty"
        );
        assert_eq!(
            rendered("schedule", Some("nightly"), "0 9 * * 1-5"),
            "schedule      [nightly] 0 9 * * 1-5"
        );
        assert_eq!(
            rendered("reply", None, "[status: \"clean\", findings: 0]"),
            "reply         [status: \"clean\", findings: 0]",
            "a subject-less act opens its payload at the verb column"
        );
        assert_eq!(
            rendered("context-evict", Some("hunter"), "3 turns"),
            "context-evict [hunter] 3 turns",
            "the longest verb still clears its column by a space"
        );
        assert_eq!(
            rendered("spawn", Some("a-name-of-the-full-24-ch"), "go"),
            "spawn         [a-name-of-the-full-24-ch] go",
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
            "cancel        [hunter] refused: not a descendant"
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
        assert!(lone.dial(Part::Thinking));

        let mut group = run("read it");
        assert!(group.dialable(Part::Run) && !group.dialable(Part::Thinking));
        let rungs = |b: &Block| match &b.kind {
            BlockKind::Group(g) => (g.thinking.at, g.run),
            _ => panic!("a group"),
        };
        assert_eq!(rungs(&group).1, Detail::Summary, "work arrives collapsed");
        assert!(group.dial(Part::Run));
        assert_eq!(rungs(&group).1, Detail::Full);
        assert!(group.dial(Part::Run));
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
}
