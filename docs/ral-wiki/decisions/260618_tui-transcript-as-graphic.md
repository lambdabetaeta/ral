---
status: proposed
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
   - **value** (the ordered lightness ramp, Bertin's quantitative variable) →
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

## Why this shape

- **The plane is reclaimed.** Position-y still defaults to time (narrative
  reading is preserved), but the rail, the matrix, and the codebase map each
  re-project the *same* `Block` buffer onto a different plane — agents × steps,
  files × recency. Bertin's thesis is that the same data re-projected reveals
  different structure; `Block`/`Viewport` already separates data from
  rendering, so the projections are architecturally free.
- **Quantitative data moves from text to value/size.** The two ordered
  variables Bertin permits for magnitude carry the magnitudes; hue is freed to
  do its associative job (agent identity).
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
  construction; hue-for-agent is not. The agent palette should be augmented
  with a shape or pattern secondary cue if agent count grows beyond two or
  three.

## See also

[[map/exarch/frontend|frontend]] (the `Block`/`Viewport`/`line` rendering arm
this re-encodes), [[decisions/260616_tool-boundary-steering|tool-boundary-
steering]] (the prompt queue whose pending strip shares the rail idiom),
[[decisions/260617_async-agent-tool|async-agent-tool]] (the subagent fan-out
the matrix makes legible), and Jacques Bertin, *Sémiologie graphique* (1967):
the plane and its six variables, ordered versus associative, reduction as
construction.
