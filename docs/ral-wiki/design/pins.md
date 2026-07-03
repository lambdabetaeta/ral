# pins: model-authored state on a register, the matrix's dual

**A pin is *state* a kit publishes to a keyed slot on a persistent register, the
in-place dual of an event appended to scrollback.** The transcript is an
append-only log of things that *happened*, sealed once written; a pin is *what is
currently true* — a task rollup, a build's status, the file under edit — and
state changes by being **overwritten**, not appended. exarch already redraws
*host*-authored state in place (the agent matrix, the `ctx%` gauge, the phase
bar — fixed marks, never streamed, never logged); a pin is the missing fourth
cell, the **same graphic grammar authored by the kit instead of the host**. The
full reasoning — why state needs its own disposition, and why position carries
the cut — is [[decisions/260622_surface-pins-state|surface-pins-state]].

## The vocabulary

A pin rides the same `surface` channel as everything else, as a *disposition
wrapper* around an ordinary [[map/exarch/cards|`` `card ``]] — the wrapper carries
only **placement**, the card vocabulary and its decoder reused verbatim:

- `` `pin [key, body] `` — write `body` to register slot `key`, **overwriting in
  place** on re-pin. The `key` is model-chosen and *is the datum's identity* —
  the thing the host cannot guess, except for the reserved `commitment:*`
  prefix below.
- `` `unpin [key] `` — drop the slot (a finished plan clears its gauge). An
  **absent body** is the same as `` `unpin ``.
- `commitment:*` — protected commitment state
  ([[decisions/260703_protected-commitment-pins|protected-commitment-pins]]).
  Ordinary `surface` calls cannot write or clear these slots; writer/verifier
  results are projected by the host into the same register.

So a kit holding evolving state pins one rollup and overwrites it, rather than
marching `tasks 0/3`, `tasks 1/3`, … down the scrollback — the streaming the rail
doctrine forbids. `exarch/data/agent.ral` (the tasks section) is the first client: `transition` pins one
gauge that *fills in place*, and a per-task `open → done` move appends nothing.

## How it flows

One decoder, one new arm, one viewport field — no concurrency invariant, because
a pin is emitted **in-turn** through the live foreground sink, the exact place a
card is already safe:

- **Decode.** `value_to_pin` is tried first on the surface channel, ahead of the
  `io` and `card` arms ([[map/exarch/shell-eval|shell-eval]]); they cannot collide
  — `io` is a `Map`, the rest distinct `Variant` labels. It resolves to
  `Kind::Pin { key, card }` or `Kind::Unpin { key }`.
- **Register.** The slots live on the [[map/exarch/frontend|`Viewport`]] as an
  *ordered* `key → Card` map; a pin is `set_pin` (overwrite or insert, first-seen
  order), an unpin is `drop_pin` — the in-place analogue of `push_card`, touching
  neither the flatten nor the log. `reset` clears it on `/clear`, so pins are
  generation-bounded exactly as scrollback is.
- **Render.** The register is a **reserved right-hand column** for the *focused*
  session — a flat strip glued to the right edge, never a floating overlay that
  would occlude the yank-able scrollback. It claims only dead margin past the
  `READ_W` reading cap, so the transcript never narrows; below a width threshold
  it collapses to a one-row **pin band** beside the matrix. The frame reads
  symmetrically: the **rail owns the left edge** (what happened), the **register
  the right** (what is).
- **Headless.** There is no register to overwrite, so a pin neither prints nor
  logs — `events.json` records only events. Pinned state is ambient, like the
  matrix.

## The model watches its own pins

Because pinned state is something the *user is watching* on the rail, the
[[map/exarch/agent|nudge]] facility keeps the model restless about it. The
agent keeps a small `key → one-line summary` mirror of the pins as they flow
past — typed by pin kind, while the session is otherwise pin-blind and the
events go straight to the frontend. There is **one** pinned-state nudge,
uniform for every pin kind (a task, a goal, a protected `commitment:*` pin
alike) and every agent role (the interactive trunk and a returning sub-agent
alike): while anything is pinned, a **budget-free** reminder fires on every
clean completion, naming the pinned state; with nothing pinned, a gentler,
throttled reminder suggests `set-goal`/`add-task` instead. This nudge is
independent of, and additive with, a *returning* agent's separate obligation
to call `reply` — neither suppresses the other, so a sub-agent that finishes
without replying while it still holds a live commitment is reminded of both at
once. This is the discipline pinning earns: a kit that publishes state to a
slot the user watches is reminded to keep it true, and the host never lets a
protected obligation go quiet just because the agent holding it happens to be
a sub-agent rather than the trunk.

The actor can request a check on a live commitment through
`verify_commitment`, whose input is only the protected key; the host reads the
saved card, builds the verifier prompt itself, and launches an `amnemon`
verifier the same launch-only, always-asynchronous way `amnemon`/`mnemon`
launch any other child. The pin clears — on the host's own thread, as the
verifier's settled result drains — only on a matching structured pass
verdict; nothing else, and no one but a verifier, ever clears one.

## Why this shape

- **It is the next honest cut on the road already taken.** documents → (operation
  vs appearance) → (render vs control) → **(event vs state)**: each step refined
  the [[design/exarch-architecture|surface]] taxonomy by one distinction, and this
  is the first that looks *inside* the render class.
- **It is the dual of a thing already on screen.** The register adds no new visual
  vocabulary — it lets a kit author what the host authors in the matrix
  ([[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]]).
- **Position carries the distinction.** Rendering state in a reserved column makes
  the plane's *horizontal* axis — Bertin's strongest variable — the event/state
  partition, legible in *where the mark sits* rather than implicit in which list
  it lands on.
- **It makes the doctrine expressible.** "Encode the changing datum as a
  fixed-position magnitude, never streamed" was enforceable only on host marks; a
  kit had no way to obey it. Pin extends it to kit state and turns the task-management kit
  from the doctrine's counterexample into its first exemplar.

## See also

[[decisions/260622_surface-pins-state|surface-pins-state]] (the decision and its
full reasoning), [[decisions/260619_surface-carries-documents|surface-carries-documents]]
(the `` `card `` body a pin reuses verbatim),
[[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]] (the matrix
this is the model-authored dual of, and the encode-don't-stream doctrine),
[[map/exarch/cards|cards]] (the render document the body decodes through),
[[map/exarch/frontend|frontend]] (the viewport register and the draw layout),
[[map/exarch/shell-eval|shell-eval]] (the host sink and the pin-first decode),
[[map/exarch/agent|agent]] (the nudge that reminds the model of its pins),
[[decisions/260703_protected-commitment-pins|protected-commitment-pins]], and
`exarch/data/agent.ral` (the tasks section — the first client).
