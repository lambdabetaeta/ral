---
status: active
---

# The transcript is a graphic, not a log

**The scrollback is re-projected as an information graphic whose variables are
encoded per-`Block` rather than smeared into prose, so the session's structure
becomes legible at a glance.** The default vertical-time view is retained as
*one* projection of the `Block` buffer; two further projections — a session
matrix and a codebase map — become cheap because the variables already live on
each block. The keystone is a data-encoding marginal rail that turns the left
two columns into a scannable thumbnail of the whole session.

This is a design proposal, not a landed change. It restates the rendering arm
of the typed `bus::Kind` dispatch ([[map/exarch/frontend|frontend]]) in the
vocabulary of Jacques Bertin's *Sémiologie graphique*: the plane and its six
visual variables (shape, value, size, hue, grain, orientation).


> **Landed: Phases 0–7.** The per-`Block` substrate (agent slot, magnitude,
> `RailShape` discriminant), the data-encoding marginal rail (Move 1), the
> `rule_line` ctx% value-ramp (Move 3 — the Gantt phase ribbon that shipped
> alongside it was subsequently retired for an elapsed-wait bar; see the
> amendment under Move 3), the collapsed-header
> size bars (Move 4) and diff-density grain (Move 5), graded reduction (Move 6),
> the agent×step matrix (Move 2), and coherent degradation (Move 7) are
> implemented in `exarch/src/tui/`. Only Phase 8 (the projection switch — the
> codebase map and a keybinding to cycle projections) remains `proposed`. A later
> amendment under Move 6 (2026-06-20) proposes coalescing observation-only `ral`
> calls into a dialable *block* (a live-tip L1, no L0), with each `diff` an
> always-visible barrier between blocks, and is not yet built.
## The diagnosis

The present TUI is a *narrative log*:

- `Viewport` holds `Vec<Block>` in arrival order; rendering is a pure
  list-flatten (`tui/viewport.rs::Flat`), so **position-y encodes time and time
  alone** — the most powerful variable, the 2D plane, is used in one dimension.
- Categorical identity (block kind, producing agent) is encoded in **hue**, the
  variable Bertin classifies as *associative but not ordered*: it distinguishes
  without ranking, so it cannot carry magnitude.
- The actual quantitative data — token cost, lines changed, phase duration —
  sits as **literal digits** in `rule_line` and the patch header. The eye reads
  differences; the differences are buried in text that must be parsed.
- The `❖` rail (`tui/line.rs::RAIL = "❖ "`) is *decorative*: every block wears
  the same glyph. Two columns of margin carry zero information.
- `rule_line`'s `─` filler is non-data ink; its spinner uses **motion**, the
  variable Bertin distrusted — motion demands attention without encoding
  magnitude.
- Disclosure is binary (`tool_call_collapsed` / `tool_call_expanded`); a
  2-line grep and a 2000-line read render at identical visual weight.

Bertin's charge would be precise: the graphic possesses the variables to carry
the data and declines to use them.

## The decision: seven moves

Moves 1–6 bind one Bertin variable to one semantic dimension, on data the
`Block` already holds. Move 7 extends the same principle — encode signal in the
rendering medium rather than hiding it — into the *epistemic* register, where
the signal is the model's own reliability. They compose; moves 1–6 need no new
event on the bus, while move 7 can deepen with one.

1. **The rail becomes a marginal index.** The left two columns, today uniform
   `❖`, encode three variables at once:
   - **shape** (associative) → block *kind*: `▎` patch, `◆` tool call, `·`
     markdown, `━` step boundary, `✗` error;
   - **hue** → the *producing agent* (root / each subagent's palette slot);
   - **value** (the ordered lightness ramp — *ordered*, not quantitative, in
     Bertin's classification: rankable but not read as a numeric ratio) →
     *magnitude*: lines changed for a patch, tokens for a tool call, wall-time
     for a phase.
   The rail becomes a 2-column thumbnail of the whole session — its shape is
   the session's shape. This is the keystone: every other move composes on it,
   because the variables are encoded per-`Block` rather than woven into text.

2. **The tab bar becomes a reorderable matrix.** `tab_bar` (1D list) is
   replaced, when more than one session is live, by Bertin's reorderable
   matrix: rows = agents, columns = wall-clock steps, cell glyph = state
   (running / done / failed) with **value** = cumulative token spend and
   **size** = lines touched. Reorder rows by spawn-time *or* by cost — the
   reordering *is* the analysis: which child burned the budget, which ran
   longest, which spawned which. Collapses to the existing bar when only root
   is live.

3. **`rule_line` earns its data-ink.** Replace the motion spinner and `─`
   filler with two ordered variables:
   - a **value ramp** bar for `ctx%` (lightness steps toward the cap, not the
     bare `N%` digit) — the eye reads the fill level and *notices the approach
     to full*, which is the one fact the gauge exists to surface;
   - a **Gantt ribbon** of completed phases, segment width = duration, value =
     token cost, the live phase as the bright tip — a session-history sparkline
     for free.
   Keep the digits as a precise readout *beneath* the bar: the graphic gives
   the comparison, the legend gives the value.

   > **Amended 2026-06-19 — the Gantt ribbon is retired, the ctx% ramp stays.**
   > Lived with, the ribbon failed on this ADR's own Bertin grounds. It encoded
   > a near-constant quantity: per-phase duration history barely varies across
   > agent turns — every turn is dominated by "waiting for model", so the
   > ribbon's shape (a few bright blocks, a scatter of flecks) was the same
   > every turn, and a visual variable that does not vary carries no
   > information. Constant ink is decoration — the exact charge §"The diagnosis"
   > levels at the motion spinner. Worse, it *clipped* the quantity that does
   > vary: the ribbon drove its lightness through `value_step`, calibrated for
   > *line counts* (buckets at 4/20/80), on *millisecond* inputs, so every phase
   > running in seconds pinned past the 81 ms threshold to the brightest step.
   > The ramp was saturated at the baseline, with no headroom left for the
   > abnormal case (a stall, a heavy compaction) to flare — absolute wait time,
   > the one figure you would act on, was thrown away. And it churned: a 2 Hz
   > pulsing tip plus a full-width reflow on every phase boundary made the row
   > change too fast to read while saying little.
   >
   > It is replaced by an **elapsed-wait bar** — *size-for-magnitude*, the
   > Bertin-legal idiom, whose magnitude happens to be live:
   > - A single fixed-width bar encodes elapsed wall-time on the *current* phase,
   >   resetting when the next phase starts (i.e. per model round-trip). Size
   >   (log2 fill) and value (a new seconds-calibrated `wait_step`: dim below
   >   ~10 s, flaring past ~30 s) both encode elapsed, so the common case stays
   >   quiet and a dragging wait flares — restoring the headroom the saturated
   >   ribbon lacked.
   > - An `Ns` seconds readout ticking once per second: a calm 1 Hz liveness
   >   signal — the bar ceasing to grow means the turn has wedged — replacing the
   >   2 Hz pulse.
   > - Rendered in `PURPLE`, distinct from the ctx ramp's `CYAN`.
   >
   > The per-`Viewport` `PhaseSeg` / `phase_history` machinery (Phase 2) is
   > removed; `value_step` is left untouched (the new bar runs on `wait_step`),
   > so the rail's line-count ramp is unaffected. The ctx% value-ramp is kept as
   > landed.
   >
   > The lesson, in one sentence: Bertin's distrust of motion is scoped to the
   > *analytical*, read-at-rest register (maps, matrices), whereas a live status
   > line also serves a *monitoring* register — "is it alive, and how long has
   > this taken?" — in which a slowly-growing magnitude is legitimate, not the
   > decorative motion Bertin (and this ADR) rightly reject.

4. **Size as a quantitative variable on collapsed blocks.** A 500-line patch
   and a 2-line patch render at identical weight today (`patch()`,
   `tui/line.rs`). On the collapsed summary, render a bar whose width ∝
   `log(lines_changed)` beside the path; the transcript becomes self-describing
   — you scan and *see* the big events without reading. Same for tool calls: a
   2000-line read versus a 1-line grep differ at a glance.

5. **Grain for diff density.** The collapsed patch summary encodes the +/- ratio
   as a **texture** — a run of braille cells `⣿⣶⣤⣀` whose density ∝ addition
   ratio. "Mostly additions" / "balanced rewrite" / "mostly deletions" reads
   pre-attentively. This is the one place a TUI surpasses a pixel grid:
   terminal cells map cleanly onto Bertin's grain variable.

6. **Graded reduction, not binary disclosure.** Replace the open/shut toggle
   with Bertin's *construction by successive reduction*: Level 0 = rail glyph
   alone (magnitude), Level 1 = glyph + summary line, Level 2 = summary + ±N
   lines of context, Level 3 = full source. The wheel on the rail dials detail
   instead of a click flipping state. Reduction *is* the interaction — the user
   constructs the view by removing detail, the way one reduces a matrix to find
   its structure.

   > **Amended 2026-06-20 — the unit of reduction is the observation *block*; a
   > write is the barrier that breaks it.**
   > Move 6 dials a single `Block`, but the noise it must quiet is a *burst* of
   > `ral` calls, and a call's file I/O is not a sibling of the next call — it is
   > that call's own effect. Each call's reads, execs, and writes surface as their
   > own blocks ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-
   > execs]]) and land *between* the calls. And each `ral` call is its own provider
   > round-trip, so the turn loop emits a `Kind::Step` (the `━` boundary) between
   > consecutive calls as well: the real arrival order is
   > `Step · ToolCall_N · io_N · diff_N · Step · ToolCall_{N+1} · …` (the io and patch
   > buffers flush at the call's own result through `flush_surfaces`;
   > `set_result_size` searches back past them). So coalescing "a contiguous run of
   > `ToolCall` blocks" is the wrong cut twice over: the blocks are not contiguous,
   > and folding across them would swallow the reads and writes — the loudest signal.
   > The first build read the formula without the `Step` and so never coalesced
   > anything: every interior `Step` cut each burst back to a single call (fixed by
   > bridging, below).
   >
   > The transcript instead reads as `block · diff · block · diff · …`. A **block**
   > coalesces a run of observation-only calls (those whose effects are only reads,
   > greps, and execs) into one dialable object; a **diff is the barrier** — it ends
   > the current block, renders as its own always-visible block (keeping its own
   > header↔hunks dial), and a fresh block starts after it. A write is the
   > codebase's audit trail and can never be folded away; observation is recoverable
   > context and compresses hard. The interior `Step` boundaries are neither: a step
   > is provider bookkeeping, not content, so the run **bridges** it — a `Step`
   > between two observations is subsumed into the block and never drawn, while a
   > `Step` at a run's edge (before genuine content, or live at the tail before the
   > next call lands) still renders as the `━` boundary it is. The matrix's
   > per-`Step` columns (Move 2) read the unchanged block buffer, so bridging is a
   > render-time decision only. This is a *projection*, not a stored object
   > (§"Why this shape": the same buffer re-read) — the flatten groups what arrival
   > order already adjoins, so push, `user.log`, aggregation, and `set_result_size`
   > are untouched. (A deliberately `surface`d `text`/`fields`/`measure` card is the
   > model's own communication, not an effect, and stays a standalone block.)
   >
   > The block carries a three-level dial — **there is no L0; L1 is the floor:**
   > - **L1, the live tip.** One line — the *latest* call's intent and the
   >   sparkline (Move 4's size bars drawn *vertically*, one bar per call,
   >   `▁▂▃▄▅▆▇█`, sequence on x and magnitude on y; the bar count *is* the call
   >   count, so the `×N` digit is dropped) — then, on the next line, that latest
   >   call's reads / greps / execs, coalesced. Only the newest call shows as text;
   >   every earlier call is its bar alone, and the line refreshes to the newest
   >   call as the block grows. It is a monitoring read (cf. the Move 3 amendment):
   >   what the model is doing *now*, over the shape of what it has done.
   > - **L2, the full list.** The block unfolds to every call — each with its
   >   intent, its reads/greps/execs, and its bar.
   > - **L3, everything.** The above plus each call's full ral source.
   >
   > The sparkline obeys the Gantt lesson (Move 3 amendment): it is ink only where
   > the magnitudes vary; a flat burst still earns its single live-tip line and its
   > de-duplicated `ral` token, just not a legible bar profile. Two boundaries
   > against the rest of the ADR: magnitude rides the bars, **distress does not** —
   > fidelity (Move 7) modulates the *intent line's* value, never a bar's height, so
   > a short bar can never be misread as a stressed call. And this is a TUI
   > projection only — headless prints each call and its effects in arrival order,
   > unchanged.

7. **Coherent degradation: the interface's fidelity tracks the model's
   capability.** Every agent TUI renders a degraded answer — one emitted under
   context pressure, after retries, or with low confidence — identically to a
   sound one, so the user trusts both the same. The interface lies about
   reliability. Instead, let the rendering *degrade with* its source:
   context-stressed responses render in a dimmer, lower-contrast typeface;
   low-confidence ones waver (a subtle value oscillation); retried or
   self-corrected passages carry a faint grain. The signal the model *has*
   — logprobs, retry count, `last_input` against the context cap — drives the
   rendering medium itself, not an icon beside it. The interface becomes
   self-aware of its own unreliability: you stop trusting a confident-looking
   answer that came from a degraded state. This extends the per-`Block` encoding
   beyond Bertin's planar variables into the *epistemic* register — the medium
   carries how-much-to-trust, not just what-happened. `last_input` against
   `context_window` (already on `App`) is the seed signal; logprob or
   retry-count exposure on `Kind` would deepen it.

   > **Amended 2026-06-21 — the two media move off the value channel; the
   > waver is retired.** As first built (Phase 7 below), context pressure
   > degraded prose with `Modifier::DIM` plus a contrast pull toward the
   > background, and echo wavered alternate rows ±1 value-step. Lived with,
   > both failed on this ADR's own grounds:
   > - **`DIM` + the background pull *is* the minor-chrome idiom.** The same
   >   treatment renders help text, stop reasons, and the watching-hint
   >   (`line::dim`), so a degraded answer — important, to be read warily —
   >   converged on the exact look of ignorable chrome. The learned reading
   >   "dim = minor" inverts the intended "dim = distrust this."
   > - **It spent the value channel on distrust.** Pulling the foreground
   >   toward the background lowers luminance — the very channel the rail and
   >   the bars reserve for *magnitude* (the "encode magnitude in hue"
   >   alternative was rejected precisely to keep value free). A degraded
   >   passage and a small one then drift toward the same lightness, and the
   >   text that most warrants scrutiny becomes the hardest to read.
   > - **The waver is motion-adjacent.** Per-row value oscillation in static
   >   prose reads as a render glitch, the decoration §"The diagnosis" and the
   >   Move 3 amendment reject — and it, too, lived on the value channel.
   >
   > The two signals are re-encoded onto two disjoint colour axes, **neither
   > the value channel:**
   > - **Context pressure → foreground saturation drain.** Each foreground
   >   mixes toward the grey of its *own* Rec. 601 luma (`rail::desaturate`),
   >   by ~45% / ~70% / ~90% at levels 1–3. Luminance is held, so the prose
   >   loses its hue — "drained of confidence" — yet stays as legible as the
   >   sound answer beside it, and never collides with magnitude. No `DIM`.
   > - **Echo → flat background wash.** A static `ECHO_WASH` field sits behind
   >   every span, deepening one step per echo level — a flagged passage, not
   >   a glitch. Background axis, apart from the drain's foreground axis, so
   >   the two signals stay separable (the reason `Fidelity` keeps them
   >   distinct). Grain stays the reserved slot for a future third signal.
   >
   > The exact hues, drain depths, and `ECHO_WASH` value are a first cut and
   > **provisional — the final visual is the owner's call**, to be judged
   > against real degraded turns in a live terminal. What is settled is the
   > principle: distrust rides saturation and the background field, never the
   > lightness that means magnitude. (The Phase 7 plan below still records the
   > original DIM/pull/waver build; this amendment supersedes its "Rendering
   > modulation" bullet.)

## Why this shape

- **The plane is reclaimed.** Position-y still defaults to time (narrative
  reading is preserved), but the rail, the matrix, and the codebase map each
  re-project the *same* `Block` buffer onto a different plane — agents × steps,
  files × recency. Bertin's thesis is that the same data re-projected reveals
  different structure; `Block`/`Viewport` already separates data from
  rendering, so the projections are architecturally free.
- **Quantitative data moves from text to value/size.** Magnitude rides value
  (Bertin's *ordered* variable) and size (his one *quantitative* variable —
  though we `log2`-scale every bar, so even size reads as ordered here, a rank
  rather than a ratio); hue is freed to do its associative job (agent identity).
  Either way the digits remain the quantity: the graphic gives the comparison,
  the legend gives the value.
- **The marginal index costs two columns.** Nothing in the present layout
  depends on the rail being uniform; `wrap_line` and the `line::plain`
  rail-stripping already treat the rail as separable chrome, so the copy path
  is unaffected.
- **Multi-agent legibility.** The matrix is the move that makes a fan-out of
  subagents readable — today a 1D tab bar loses the global picture the moment a
  second agent is born.
- **The interface becomes honest about reliability.** Coherent degradation
  makes the medium carry the model's own confidence, so a degraded answer no
  longer borrows the visual authority of a sound one. This is the move that
  cannot be argued against on Bertin's grounds — it is not a better encoding of
  *what happened*, but the first encoding of *whether to trust it*. The seed
  signal (`last_input` vs `context_window`) is already on `App`; the rendering
  cost is a style modulation on the existing `Markdown`/`Chrome` paths.

## Sequencing

Move 1 (the data-encoding rail) is the keystone and the first to build: it is a
localised change to `tui/line.rs`'s rail construction, it makes the session
scannable on its own, and it encodes the per-block variables every other
projection reads. Move 3 (`rule_line`) is independent and can land in parallel.
Moves 2 and 4–6 all consume the variables move 1 establishes, so they follow.
Move 7 (coherent degradation) is independent of the rail and can land at any
point; its seed signal is already present, and deeper signals (logprobs,
retry count) arrive as `Kind` extensions without disturbing the other moves.

## Alternatives considered

- **Keep the log, add a separate dashboard pane.** Rejected: it duplicates the
  data and forces a split focus. Encoding variables per-block makes the
  *existing* transcript the graphic; no second surface is needed.
- **Encode magnitude in hue (a "heat" colour ramp).** Rejected: hue is
  unordered, so a heat ramp is read only by convention and collides with hue's
  job of carrying agent identity. Value (lightness) is the ordered variable and
  does not collide.
- **Motion for magnitude** (a faster spinner for heavier work). Rejected on
  Bertin's grounds: motion demands attention without ranking, and it cannot
  persist — a stopped phase's magnitude would vanish. Value and size persist
  and are comparable across the whole buffer at rest.

## Open questions

- **Rail glyph set.** The shape vocabulary above is illustrative; the final set
  must be distinguishable at one cell, copy-strippable (the `line::plain` /
  `RAIL_GLYPHS` contract must extend), and consistent with the disclosure
  triangles `▸`/`▾` already in use.
- **Magnitude source for tool calls.** Patches carry `Hunk` line counts
  directly; tool calls do not yet carry a token or byte figure on the bus.
  Move 1's value-ramp on tool calls may need a `Kind` extension or a derivation
  from `Kind::Usage`, or else defer tool-call magnitude to move 4's size bar
  keyed off the result.
- **Degradation signal source.** Context pressure is derivable today; a richer
  fidelity ramp wants logprob aggregates, retry counts, or self-correction
  detection on `Kind`. The open question is which signal the provider exposes
  cheaply enough to drive per-block rendering without stalling the stream, and
  whether to degrade the *whole* turn (one fidelity) or per-paragraph (the
  agent's confidence shifts mid-answer). Per-paragraph is more honest but needs
  a stream-aligned signal; turn-level is the cheap seed.
- **Projection switching surface.** The three projections need a keybinding and
  a default. The matrix and codebase map are views *over* the same buffer, not
  separate modes with separate state — the open question is the gesture, not
  the data model.
- **Colour-blind safety.** Value (lightness) and size are colour-blind safe by
  construction; hue-for-agent is not.

  > **Amended 2026-06-21 — the agent palette is made CVD-safe by lightness, not
  > a second channel.** A secondary shape/pattern cue cannot ride the rail: its
  > single cell already spends shape on *kind* and value on *magnitude*, so a
  > fourth per-agent variable would over-pack it. Instead `AGENT_HUES`
  > (`line.rs`) is rebuilt so the six hues separate by a *combination* of
  > lightness and hue rather than hue alone — a descending `L*` ladder under
  > which every pair stays distinct in simulated deuteranopia and protanopia
  > (worst-case ΔE76 ≈ 19, against ≈3 for the old hue-only set, where
  > orange/red and pink/lime collapsed). Root stays the exact `CYAN` accent, so
  > a root-only session is unchanged. This fixes the one hue-only surface; agent
  > identity is already recoverable as **text** in the matrix rows and tab
  > labels, so those carry no hue-only risk. A *design call* for the owner: the
  > new non-root hues (amber, magenta, blue, olive, plum) widen the lightness
  > range beyond the old near-isoluminant set, which is the cost of the CVD
  > separation.

## Implementation plan

The seven moves decompose into eight phases. Phase 0 builds the per-`Block`
substrate every move reads; Phase 1 is the keystone rail; Phase 2 is independent
and parallel; Phases 3–6 consume the variables Phase 1 establishes; Phase 7 is
freestanding; Phase 8 is the projection switch. Each phase is a landable parcel
with its own test.

**No new state on `App`.** Every piece of new state is either derivable at
render time or session-scoped — and session-scoped state belongs on `Viewport`,
which already owns the block buffer, scroll position, and log. `App` is the
dispatch layer; it routes events to viewports. The current code violates this
split by keeping `total_usage` and `phase` on `App` (they work only because root
is the common case). The phases below push per-session figures to `Viewport`
and leave `App` unchanged except for handler dispatch — one extra method call
per `Kind`.

```
0  Substrate        per-Block agent + magnitude; rail-lift
1  Move 1           data-encoding marginal rail (keystone)
2  Move 3           rule_line value-ramp + Gantt ribbon        ∥ 1
3  Move 4           size bars on collapsed blocks
4  Move 5           grain for diff density
5  Move 6           graded reduction (disclosure levels 0–3)
6  Move 2           agent×step matrix
7  Move 7           coherent degradation
8  Projections      codebase map + switch keybinding          *(for later)*
```

Three forks, settled for Phase 1: the rail is **lifted to a per-`Block` method**
(not extended in each builder); **every block kind carries the 2-col rail**
(markdown included, replacing its bare indent); **step boundaries render a
visible `━`** rail marker (overriding the blank-line-only convention).

### Phase 0 — substrate

The `Block` gains the two variables the rail encodes; the `Viewport` supplies
them. `App` changes only its `Kind::Born` handler to pass a slot by value.

- **`AgentSlot`** (`tui/block.rs`): a `u8` index into a rail palette. Root = 0.
  Subagents are assigned the next index on `Kind::Born` — `slot =
  (self.tabs.len() as u8) % AGENT_HUES.len()` at the moment the viewport is
  created — wrapping modulo the palette length. No `HashMap` on `App`: the
  slot is passed by value through `Viewport::new(log_path, agent)` and lives on
  the viewport. The rail (Phase 1) reads it from `Block::agent`, stamped at
  push; the matrix (Phase 6) reads it from `Viewport::agent` at render time.
- **Rail palette** (`tui/line.rs`): `AGENT_HUES: [Color; 6] = [CYAN, PINK, LIME,
  PURPLE, ORANGE, RED]`. Root stays `CYAN` — the existing rail accent — so a
  root-only session is visually unchanged in hue.
- **`Viewport::agent`**: each viewport owns its session's slot, set at
  construction (`Viewport::new(log_path, agent)`). Every `push_*` stamps that
  slot onto the block it mints. The one cross-agent block — the `SubagentDone`
  breadcrumb landed in root's scrollback — stamps root's slot (the viewport's
  own), not the child's. The child's identity is carried in the breadcrumb's
  `↘ <title>` text; the rail hue is root's because the block lives in root's
  buffer. Acceptable: the breadcrumb is root's *reception* of the child's
  result, not the child's own utterance.
- **`Block` fields**: `agent: AgentSlot`, plus a `shape` discriminant on chrome.
  `BlockKind::Chrome(Vec<Line>)` becomes `BlockKind::Chrome { shape: RailShape,
  lines: Vec<Line> }` so the rail (and moves 4–6) can dispatch on the chrome
  sub-kind without re-parsing the built lines. `RailShape = Step | Error |
  Generic` — the coarse set the rail needs; patches and tool calls derive their
  shape from their own `BlockKind` variant.
- **`Block::magnitude(&self) -> Option<u32>`**: `Some(del+add lines)` for
  `Patch`, `None` elsewhere. No `Kind` change in Phase 0 — tool-call magnitude
  defers to Phase 3 / a later `Kind` extension (the open question stands).

### Phase 1 — Move 1, the data-encoding rail

A new `tui/rail.rs` module owns the three-variable encoding; `Block::render`
prepends the rail span to the block's first visual row, and nothing else.

- **`rail::span(shape, agent, magnitude) -> Span<'static>`** returns
  `Span::styled("{glyph} ", fg(lighten(AGENT_HUES[agent.0], value_step(magnitude))))`.
  One cell carries shape (the glyph), hue (the agent palette), and value
  (lightness toward white) — the keystone trick. The second column stays a
  space, as today.
- **Shape map**:
  - `ToolCall { open: false }` → `▸`, `open: true }` → `▾` — the disclosure
    triangle *is* the tool-call shape (no separate `◆`).
  - `Patch` → `▎`; `Markdown` → `·`; `RailShape::Step` → `━`;
    `RailShape::Error` → `✗`; `RailShape::Generic` → `❖`.
- **`value_step(magnitude: Option<u32>) -> u8`** thresholds into `0..=3`:
  `None` → 0; `Some(n)` bucketed on `log2` (≈ 1–4 → 0, 5–20 → 1, 21–80 → 2,
  81+ → 3). Patches are the sole carrier in Phase 1; all other blocks render at
  step 0 (base hue).
- **`lighten(c: Color, step: u8) -> Color`** interpolates each RGB channel
  toward 255 by `step / 3`, so step 3 is near-white. Brighter rail = larger
  event; the ramp is comparable across the whole buffer at rest.
- **Rail lift.** `Block::render` builds the body lines via the existing
  `line::*` builders (now rail-less — they lose their hardcoded `RAIL` / `▸` /
  `▾` first span), then prepends `rail::span(...)` to `lines[0]` only. Body rows
  stay rail-less, exactly as today. `Block::log_lines` routes through the same
  path with the tool call forced open, so `user.log` carries the new rail too.
- **Builder changes.** `line::patch`/`wrote`/`task`/`meter`/`user_prompt`/
  `queued_prompt` drop the leading `RAIL` span. `tool_call_header` drops its
  `▸`/`▾`. `error` lifts `✗` out of the content span into the rail (content
  becomes `error <msg>`). `step` keeps its single blank body line; the `━` rail
  on that row is what makes the boundary visible.
- **Markdown reconciliation.** `md::render_md` keeps its `MD_INDENT` inset on
  every body row; the `·` rail prepends to the first row only. The first row's
  text column is aligned to the body's by indenting the first row `MD_INDENT −
  rail_width` after the rail, so prose stays flush across the block.
- **Copy contract.** `RAIL_GLYPHS` (`tui/line.rs`) extends to the full shape
  vocabulary — `["▎ ", "▸ ", "▾ ", "· ", "━ ", "✗ ", "❖ "]`. `plain` is
  unchanged in spirit: drop a leading span whose content is a known rail token.
  Selection and yank continue to copy rail-stripped text.
- **Tests.** `line::tests` asserts `ls[0].spans[0].content == RAIL` and on
  `plain()` output (`patch_numbers_gutter…`, `queued_prompt_*`); these move to
  the post-lift rail and are re-grounded on `rail::span`. `viewport::tests`
  `flatten_text` assertions update for the new first-row glyphs. A new test
  pins the three-variable contract: a large patch renders a brighter `▎` than a
  small one at the same agent hue.

Phase 1 lands the keystone: the rail becomes a 2-column thumbnail whose shape is
the session's shape, and the per-`Block` variables every later projection reads
are in place. Phase 2 (Move 3) is independent and may proceed in parallel.

### Phase 2 — Move 3, rule_line

Independent of Phases 0–1: it reworks `rule_line` (`tui.rs:1214`) and the
status row layout, not the rail. The motion spinner and `─` filler are replaced
by two ordered variables — a value ramp for `ctx%` and a Gantt ribbon of
completed phases — with the digits kept as a precise readout beneath the bar.

**Layout reorder.** Today the vertical layout (`tui.rs:612`) is transcript →
blank → tabs → *rule_line* → queued → prompt → footer. Phase 2 moves `rule_line`
to sit immediately above the footer hint, below the prompt — so the two
bottom rows are status then hints, with the prompt above both. The constraint
vector and the destructure at `tui.rs:622` reorder; nothing about `rule_line`'s
body depends on its row.

```
before:  transcript · blank · tabs · rule_line · queued · prompt · footer
after:   transcript · blank · tabs · queued · prompt · rule_line · footer
```

- **Value-ramp `ctx%` bar.** The bare `ctx N%` digit (`tui.rs:1251`) becomes a
  fixed-width lightness ramp (≈10 cells): filled cells step through
  `value_step(pct)` toward white as `last_input / context_window` approaches
  1.0, empty cells dim slate. The eye reads the fill level and *notices the
  approach to full* — the one fact the gauge exists to surface. The `N%` digit
  stays as a small readout beneath/after the bar: the graphic gives the
  comparison, the legend gives the value. `value_step` and `lighten` are shared
  with Phase 1's rail (`tui/rail.rs`) — same ramp, same scale.
- **Gantt phase ribbon.** A row of segments, one per completed phase, segment
  width ∝ wall-time duration, the live phase as the bright tip. Replaces the
  `─` filler: the filler was non-data ink; the ribbon is a session-history
  sparkline for free. Needs **new state on `Viewport`**, not `App`: a
  `Vec<PhaseSeg { label, duration }>` accumulated as phases start/end. Today
  `self.phase: Option<String>` lives on `App` (`tui.rs:265`) and is set/cleared
  by `Kind::Phase` / any-other-event (`tui.rs:421`/`468`) — but phase is
  per-session, so Phase 2 moves it: `App::handle` calls `vp.set_phase(label)`
  / `vp.clear_phase()`, and `Viewport` owns both the live `Option<String>` and
  the `Vec<PhaseSeg>` history (capped to last N). `App`'s `phase` field is
  deleted; `rule_line` reads the focused viewport's phase. The live segment
  pulses subtly to preserve the liveness signal the motion spinner carried — a
  stopped phase is a solid segment, the live one breathes, so a hung turn is
  still distinguishable from a quiet one.
  - **Token cost is not per-phase today.** `Kind::Usage` fires per model
    request (`session.rs:307`), and a phase ("typechecking") is a local op
    between requests that carries no Usage of its own. So the ribbon encodes
    *duration* (width) reliably; the decision's "value = token cost" per
    segment defers until a phase-aligned cost signal exists (a `Kind` extension
    or a derivation). Phase 2 ships duration-only; value is uniform. Noted as a
    fork for a later phase.
- **Spinner disposition.** The braille `SPIN` + colour-cycle (`tui.rs:70`) is
  motion — Bertin's distrusted variable. Phase 2 drops it. Liveness migrates to
  the Gantt's pulsing live tip (above) plus the `ctx%` bar's fill level
  advancing. The phase label (`phase.as_deref()`, "typechecking…") stays as
  text beside the ribbon — it names *what* is running, which the graphic
  cannot.
- **Tests.** `rule_line` is a pure `fn` over its args; the existing test shape
  (build a `Line`, assert on spans) extends to assert the ramp fills with
  `last_input` and the ribbon carries one segment per recorded phase. The
  layout reorder is verified by the render snapshot tests in `tui.rs::tests`
  (`2031+`) if present, else by a geometry assertion on `status_row.y`.

*Amended 2026-06-19: the Gantt ribbon described here was retired in favour of an
elapsed-wait bar — see the amendment under Move 3 above.*

### Phase 3 — Move 4, size as a quantitative variable

The two collapsed headers — patch and tool call — gain a width-`log(magnitude)`
bar, the second ordered variable (size) after the rail's value (lightness).
Both read the per-`Block` magnitude Phase 0 exposes; the patch bar lands for
free, the tool-call bar costs one wiring.

- **Patch header** (`line::patch`, `tui/line.rs:290`): beside the path, a bar
  of ≈8 cells, filled cells `█` / empty `░`, width ∝ `log(del+add)`. Reuses
  `Block::magnitude` (Phase 0). A 500-line patch fills the bar; a 2-line patch
  barely shows. The bar sits on the always-visible header — patches are not
  collapsible yet, so the header *is* the summary.
- **Tool-call magnitude source.** `Kind::ToolResult(text)` is emitted for every
  call (`session.rs:613`, `tools.rs:110`, `tools/fff.rs:258`) but discarded by
  the TUI (`tui.rs:482`). Phase 3 wires it to the viewport: the `Kind::ToolResult`
  handler calls `vp.set_result_size(text)`, which captures
  `text.lines().count()` and attaches it to the most-recent `ToolCall` block —
  searched backward from the tail, since `Patch`/`Wrote` side effects may land
  between a call and its result. `Block` gains `result_size: Option<u32>` (a
  field on the block, not `App`); setting it invalidates the memo so the header
  re-renders with the bar. The collapsed tool-call header (`▸ <summary>`) gains
  the same `log` bar.
  - `fff` results are clipped to `FFF_CAP` (`tools/fff.rs:258`); their bar tops
    out at the cap — acceptable. The `agent` tool's result arrives as
    `Kind::SubagentDone`, not `ToolResult`; its bar is keyed off the
    breadcrumb's `text` length, or left at base. Small exception, noted.
- **Two magnitudes, two granularities.** The rail's value-step (one cell) gives
  the thumbnail; the header size-bar (≈8 cells) gives the readout. Both encode
  magnitude without colliding — one scans, one reads.

### Phase 4 — Move 5, grain for diff density

The patch header gains a grain run beside the size bar: ≈4 braille cells
(`⣿⣶⣤⣀`) whose density ∝ the addition ratio `add/(add+del)`. `⣿` = all
additions (full), descending through `⣶`/`⣤` to `⣀` = all deletions. "Mostly
additions / balanced / mostly deletions" reads pre-attentively — the one place
a TUI surpasses a pixel grid, terminal cells mapping onto Bertin's grain.

- Density is computed from the hunks' aggregate `del`/`add` counts — already
  summed for magnitude in Phase 0. No new data.
- Composes on the Phase 3 header: `▎ patch  <path>  <size-bar>  <grain-run>`.
  Size carries *how much*; grain carries *what kind* (add/del balance). The
  two answer different questions at a glance.

### Phase 5 — Move 6, graded reduction

The binary `open: bool` becomes a `level: u8` (0–3), and reduction — not
toggle — becomes the interaction. Levels apply to `ToolCall`, `Patch`, and
`Markdown`; chrome stays L3-only (it is already 1–few lines).

- **`Block` gains `level: u8`**, default per kind: `ToolCall` L1 (today's
  collapsed), `Patch` L3 (today's always-full), `Markdown` L3 (today's
  always-full). The defaults preserve the current rendering until the user
  dials — no regression on landing.
  - L0 = rail glyph alone (the thumbnail row); L1 = glyph + one summary line;
    L2 = summary + ±N lines of context; L3 = full source.
  - `ToolCall`: L0 `▸` only; L1 `▸`+summary; L2 summary+cmd head ±N; L3 full cmd.
  - `Patch`: L0 `▎` only; L1 `▎ patch <path>` + size bar + grain (Phases 3–4);
    L2 header + first hunk ±N context; L3 full diff.
  - `Markdown`: L0 `·` only; L1 first line; L2 ±N lines; L3 full paragraph.
- **Interaction.** `mouse` (`tui.rs:968`) hit-tests `ScrollUp`/`ScrollDown`
  against the rail column (cols 0–1) and an expandable block: a hit dials
  `level` (ScrollUp ↑, ScrollDown ↓, clamped 0–3) and consumes the event;
  otherwise the wheel scrolls as today. A click on the rail cycles L1↔L3,
  preserving today's click-toggle affordance; clicks off the rail stay
  selection. The dial is the gesture the decision's "open question: projection
  switching surface" anticipated at the block scale.
- **API.** `Block::toggle` → `Block::dial(delta: i8)`; `Viewport::toggle_block`
  → `Viewport::dial_block(idx, delta)`. Memo invalidation as on toggle today.
- **Default-level fork.** L3 for patches/markdown is conservative (no visual
  change until the user dials). The decision's spirit favours a lower default
  so the rail scans clean — but that is a visible behaviour change. Recommend
  L3 default, revisit once the rail thumbnail (Phase 1) is proven in use.

Phases 3–4 augment the always-visible patch header and the collapsed tool-call
header; Phase 5 then makes those headers the L1 view of a dialable stack. Either
order composes — Phase 5 first decorates L1 with Phases 3–4 after; Phases 3–4
first land on today's headers and Phase 5 generalises. The decision sequences
4 → 5 → 6, which we follow.

### Phase 6 — Move 2, agent×step matrix

The 1D `tab_bar` (`tui.rs:1327`) becomes a reorderable matrix when more than
one session is live, so a fan-out of subagents reads as a comparable grid
instead of a list of labels. Rows = agents, columns = wall-clock steps, cell
glyph = state (running / done / failed) with **value** = cumulative token spend
and **size** = lines touched. Collapses to the existing bar when only root is
live — no change for the common case.

**No `AgentRow`, no new state on `App`.** Every figure the matrix shows is
either already on `App` (`tabs`/`titles`/`dying`, `tui.rs:229`/`232`/`236`) or
derivable from a `Viewport` at render time. The one missing piece is per-agent
token spend — `App::total_usage` (`tui.rs:266`) is global, and the `Kind::Usage`
handler ignores `id` (`tui.rs:446`). Phase 6 pushes that to `Viewport`: the
handler calls `vp.add_usage(u)` alongside the existing `self.total_usage += u`
(which stays for `rule_line`). `Viewport` gains `usage: Usage` (one field, one
method). Everything else is a render-time read:

| figure | source | new state? |
|--------|--------|------------|
| title | `self.titles[id]` (App, existing) | no |
| hue / slot | `Viewport::agent` (Phase 0) | no |
| spawn order | `self.tabs` order (App, existing) | no |
| state (running/dying) | `self.dying` + tab membership (App, existing) | no |
| step count | count `Step` blocks in `Viewport::blocks` | no — derived |
| lines touched | sum patch magnitudes in `Viewport::blocks` | no — derived |
| token spend | `Viewport::usage` (new field) | **yes, on Viewport** |

```
main       ●○○○○  12k  ▓▓▓░░  4st
refactor   ●●●○○  3.4k ▓░░░░  2st
tests      ✓●○○○  880  ▓░░░░  1st
```

- **Row = agent.** Label (truncated `titles[id]`), hue from
  `AGENT_HUES[vp.agent]` (Phase 0) — so the matrix's row colours match the
  rail's per-block agent hue. One identity, one colour, everywhere.
- **Step cells = columns.** Derived: scan the viewport's blocks, count `Step`
  blocks; for each step, scan forward to the next step/boundary and note
  whether a `ToolCall` block sits in between (`●` = step with a tool call,
  `○` = without). `✓` = the session is done/dying, `✗` = last block was an
  error. Capped to row width minus the label and two readouts; a long run
  shows the most recent steps.
- **Value readout = cumulative tokens.**
  `fmt_tokens(vp.usage.input + vp.usage.output)` (`tui.rs:1204`), styled at a
  lightness ∝ `value_step(usage)` (brighter = more spend) — reusing Phase 1's
  ramp so "which child burned the budget" reads at a glance.
- **Size readout = lines touched.** Derived: sum `Block::magnitude()` over the
  viewport's `Patch` blocks (Phase 0's method, already on the block). A `▓`-bar
  (Phase 3's idiom, reused), width ∝ `log(lines_touched)`. Empty for agents
  that only read.
- **Dying rows** carry the `LINGER` countdown (`tui.rs:1342`) as today,
  rendered dim.

**`Viewport::usage` — the one new field.** `Viewport` gains `usage: Usage`
(`Default`), and `add_usage(&mut self, u: Usage)` called from the
`Kind::Usage` handler. The handler changes from `self.total_usage += u` to
`self.total_usage += u; if let Some(vp) = self.viewports.get_mut(&id) {
vp.add_usage(u); }` — one extra line, no `App` field. `reset` clears it
alongside the block buffer. Root's usage is its viewport's usage; the global
`total_usage` stays for `rule_line` only.

**Reordering.** The matrix is *reorderable*: the reordering *is* the analysis.
Default sort = spawn order (`self.tabs` order). A keybinding re-sorts by cost
(`Viewport::usage`) or lines touched (derived) — which child burned the budget
surfaces to the top. This is the decision's "reorder rows by spawn-time *or* by
cost".

**Collapse.** When `self.tabs.len() == 1` (root only), `matrix_bar` returns
the existing `tab_bar` output — the common case is unchanged. The matrix is a
*projection* of existing data; it does not replace the tab model, it
re-projects it.

**Tests.** `Viewport::usage` accumulation is a pure update — unit test that
`add_usage` sums correctly and `reset` zeroes it. `matrix_bar` is a pure `fn`
over `&[(SessionId, &Viewport)]` + `&titles` + `&dying` + sort key — assert
the row order under each sort, the step-cell glyphs by derived state, and that
the single-agent case matches `tab_bar`'s output (collapse equivalence).

### Phase 7 — Move 7, coherent degradation

The interface stops lending a degraded answer the visual authority of a sound
one. Two signals drive a per-`Block` **fidelity** ramp (0 = sound, 3 = most
degraded): context pressure (turn-level, already on `App`) and echo similarity
to the immediately preceding `ral` tool call (per-block, computed at commit).
The rendering medium itself degrades with its source — dimmer typeface, lower
contrast, a row-wise waver — so the user stops trusting a confident-looking
answer that came from a stressed state.

**Signal 1 — context pressure (turn-level floor).** `last_input` vs
`context_window` is already on `App` (`tui.rs:270`/`272`, set at `Kind::Usage`
`tui.rs:452`). It is a property of the whole turn, so it floors every block
committed during that turn:

```
context_floor = match last_input / context_window {
    < 0.50 => 0,  < 0.75 => 1,  < 0.90 => 2,  _ => 3,
}
```

`context_window = None` (native providers) → floor 0, no signal. This is the
decision's named seed, free.

**Signal 2 — echo similarity (per-block delta).** When the assistant's prose
closely echoes the `ral` script it just ran, the model is parroting rather than
synthesising — a rubber-stamping failure mode that context pressure induces.
High similarity → higher degradation. Computed at **block commit**, not per
token (the decision's "without stalling the stream"): when a `Markdown` block
commits in `Viewport` (`flush_complete_paragraphs` / `flush_open`,
`tui/viewport.rs:180`/`193`), look back at the most recent `ToolCall` block in
the same viewport; if its `tool == "ral"` (`tools/ral.rs:73`), compute the
similarity of the committing text to that block's `cmd` (held on the block —
free, no `Kind` change). Else no delta.

- **Similarity = word-3-gram Jaccard.** `shingles(text) -> HashSet` of
  lowercased word trigrams; `jaccard(a, b) = |a∩b| / |a∪b|`. Cheap, no deps,
  allocation bounded by text length. Both inputs capped to their first 4 KB so
  a giant script can't stall the commit.
  ```
  echo_delta = match jaccard {
      < 0.20 => 0,  < 0.50 => 1,  _ => 2,
  }
  ```
- **Direction.** High similarity = echo = *more* degraded. The defensible
  reading: a model restating its own just-run script in prose is not adding
  epistemic value. The alternative — that explaining one's script is healthy —
  would invert the sign; rejected because the *verbatim* overlap Jaccard
  measures is restatement, not explanation (explanation paraphrases, lowering
  trigram overlap).
- **cmd vs result.** The signal uses the call's `cmd` (the script the model
  wrote), free on the block. The model *sees* the result, so echoing would
  more often target the result — but `Kind::ToolResult` is discarded by the TUI
  today (`tui.rs:482`). Phase 3 wires the result back in for the size bar; if
  that lands first, the echo signal can switch to result-similarity as the
  stronger measure. cmd-similarity is the seed; noted as a fork.

**Fidelity.** `Block` gains `fidelity: u8` (default 0), set at markdown commit:

```
fidelity = (context_floor + echo_delta).min(3)
```

Turn-level floor + per-block delta — the decision's "turn-level is the cheap
seed; per-paragraph is more honest" resolved as a hybrid: context is the floor
every paragraph inherits, echo is the per-paragraph modifier.

**Rendering modulation.** `md::render_md` gains a `fidelity: u8` param (the
one insertion point: `Composer`'s `style: Style` field, `md.rs:102`, threads
through every text span via `self.style`). Chrome that renders assistant-origin
text — the `SubagentDone` breadcrumb (`tui.rs:1282`, which calls `render_md`) —
passes the child's last fidelity. Two signals, two media, per the decision's
spirit:

- **Context pressure → value reduction (dimmer, lower contrast).** Folded into
  `Composer::style` at commit:
  - level 1: `add_modifier(DIM)`;
  - level 2: DIM + interpolate span `fg` toward `bg` by ~40% (contrast
    reduction);
  - level 3: DIM + ~70% contrast pull (near-muted).
  The fixed-colour spans (code `LIME`/`CODE_BG` `md.rs:129`, headings
  `md.rs:485+`) are *not* exempt — a degraded answer degrades its code blocks
  too, so the whole block reads as one fidelity.
- **Echo similarity → waver (row-wise value oscillation).** A subtle
  lightness oscillation across the block's rendered rows: alternate rows
  shift `fg` lightness by ±1 `value_step` (reusing Phase 1's `lighten`). The
  eye reads "unsteady" without parsing. Applied in `Composer::flush_line` by
  post-mixing the row's spans when `echo_delta > 0`. This is the decision's
  "low-confidence ones waver", driven by the echo signal specifically.
- **Grain defers.** The decision's "retried or self-corrected passages carry a
  faint grain" wants a third signal (retry count / self-correction) the bus
  does not yet expose. Phase 4's braille-grain machinery could carry it when a
  `Kind` extension arrives; Phase 7 ships the two signals above and leaves
  grain as the slot for a future `Kind::Retried` or logprob aggregate.

**Scope.** Phase 7 touches `md.rs` (the `fidelity` param + the two modulations),
`viewport.rs` (compute + stamp fidelity at commit, from a `context_floor: u8`
param passed by `App`), and one line in `App::handle` (pass `context_floor`,
derived from its existing `last_input`/`context_window`). No new `App` field, no
`Kind` change required for the seed; deeper signals (logprobs, retry count)
arrive later as `Kind` extensions and deepen `fidelity` without disturbing the
rendering path.

**Tests.** `jaccard` is a pure `fn` — unit test: identical strings → 1.0,
disjoint → 0.0, a script and its prose paraphrase → < 0.2, a script and its
verbatim restatement → > 0.5. The fidelity→style path: assert a level-2 block's
text spans carry `DIM` and a pulled `fg`; assert the waver alternates lightness
across rows when `echo_delta > 0`.

## See also

[[map/exarch/frontend|frontend]] (the `Block`/`Viewport`/`line` rendering arm
this re-encodes), [[decisions/260616_tool-boundary-steering|tool-boundary-
steering]] (the prompt queue whose pending strip shares the rail idiom),
[[decisions/260617_async-agent-tool|async-agent-tool]] (the subagent fan-out
the matrix makes legible), and Jacques Bertin, *Sémiologie graphique* (1967):
the plane and its six variables, ordered versus associative, reduction as
construction.
