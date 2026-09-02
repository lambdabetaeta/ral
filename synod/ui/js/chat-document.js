// Cyclic with projector.js, and safe: every binding crossed either way is a
// hoisted function declaration, and nothing calls across the cycle during
// module evaluation — the first paint is app.js's, after the whole graph has
// loaded.  A top-level read of another module's `let` would break that.
import { render } from "./projector.js";
import { renderMarkCard } from "./card-renderer.js";

// ---- The chat document ------------------------------------------------

// The transcript's single source of truth: an append-only list of plain
// data blocks — user, assistant, system, and notice prose, surfaced
// cards, and dials.
// Everything under #transcript is a projection of this list; no block
// holds an element, and no handler writes to the transcript directly.
// An assistant block keeps the raw markdown of its whole bubble, so each
// token re-renders from the full source rather than patching rendered
// HTML.
export let blocks = [];

// The blocks still receiving input, held as data. `streamingProse` is
// the assistant block tokens append to; `openDial` the dial block calls,
// cards, and notes land in. Never both at once — each opens by closing
// the other — and closing one (dropping the ref to null) leaves the
// block itself in the document.
/** @type {{ kind: "assistant", raw: string, plain: boolean, rev: number } | null} */
export let streamingProse = null;
let openDial = null;

// Wipe the chat document back to empty and render, without touching the
// change report. Called by conversation.js's resetTranscript.
export function resetDocument() {
  blocks = [];
  streamingProse = null;
  openDial = null;
  render();
}

// The dial the current gap is streaming into, dropped: the narrowest
// write projector.js's onSynodEvent needs when a fresh token opens a new
// bubble, and conversation.js needs when an exchange or the assistant
// itself ends.
export function clearOpenDial() {
  openDial = null;
}

// Open a fresh streaming assistant bubble for the given first token: the
// write projector.js's onSynodEvent needs when a token arrives with no
// bubble already open.
export function openStreamingProse(text) {
  streamingProse = pushBlock({ kind: "assistant", raw: text, plain: true });
}

// Every mutation runs through these two and ends in the one projector
// pass. `rev` counts a block's revisions, so the projector can tell a
// current element from a stale one.
function pushBlock(block) {
  block.rev = 0;
  blocks.push(block);
  render();
  return block;
}

export function bump(block) {
  block.rev += 1;
  render();
}

// The streaming bubble's stream has closed — a card, a dial, or the
// exchange itself — so this is its last word: flip it out of plain-text
// mode and bump once more, so it settles as markdown rather than
// freezing mid-stream in the cheap rendering.
export function closeStreamingProse() {
  if (!streamingProse) return;
  streamingProse.plain = false;
  bump(streamingProse);
  streamingProse = null;
}

export function addUserMessage(text) {
  pushBlock({ kind: "user", text });
}

export function addSystemMessage(text) {
  pushBlock({ kind: "system", text });
}

// A neutral notice, not a failure — e.g. the large-folder warning at
// conversation start. Same shape as a system message, raised tint
// instead of danger.
export function addNoticeMessage(text) {
  pushBlock({ kind: "notice", text });
}

// A surfaced card is the work's output — a deliberate act that stands
// in the transcript at the assistant's rank. It closes both streams:
// the next token opens a fresh bubble below it, the next call a fresh
// dial, so blocks keep true temporal order.
export function addCardBlock(marks) {
  if (!marks.length) return; // an empty card would render as bare chrome
  closeStreamingProse();
  openDial = null;
  pushBlock({ kind: "card", marks });
}

// The dial: one per gap between prose bubbles. Tool calls, harness
// calls, their process cards, and warn/bad notes land in it, in true
// temporal order, as a growing list of `entries`; muted telemetry
// folds in as `notes`.
//
// The dial holds process only — surfaced cards stand in the transcript
// as blocks of their own. The rungs reveal the process by degree:
// nothing (the tip alone), the step intents, then the scripts, process
// cards, and telemetry too.

// Rung 0 needs no label: its tip already names the latest intent.
const RUNG_LABELS = ["", "all steps", "all scripts"];

// Open a dial for this gap, or hand back the one already open. A fresh
// dial is pushed without a render of its own: it enters the document in
// the caller's bump, together with the entry that opened it, so it never
// arrives as an empty shell. Opening also ends the current bubble's
// stream — the next token starts a new bubble below the dial.
function ensureDial() {
  if (openDial) return openDial;
  closeStreamingProse();
  openDial = { kind: "dial", entries: [], notes: [], rung: 0, helpers: 0, rev: 0 };
  blocks.push(openDial);
  return openDial;
}

function rotateDial(block) {
  block.rung = (block.rung + 1) % 3;
  bump(block);
}

// The collapsed tip names the latest happening — a step's intent, or a
// warn/bad note's own text when that is the newest fact — so a retry or
// provider error is never silent, even before the body is dialed open.
function dialHeaderText(dial) {
  const latest = dial.entries.findLast((e) => e.intent || e.text);
  return latest ? latest.intent || latest.text : "";
}

// The dial element, rebuilt whole from its block at every revision. The
// head's handlers write to the block and bump — never to the element.
export function renderDialBlock(block) {
  const el = document.createElement("div");
  el.className = "dial";
  el.dataset.rung = String(block.rung);

  const head = document.createElement("button");
  head.type = "button";
  head.className = "dial-head";
  head.setAttribute("aria-expanded", String(block.rung > 0));
  head.addEventListener("click", () => rotateDial(block));

  const mark = document.createElement("span");
  mark.className = "dial-mark";
  mark.setAttribute("aria-hidden", "true");
  const tip = document.createElement("span");
  tip.className = "dial-tip";
  tip.textContent = dialHeaderText(block);
  const rungLabel = document.createElement("span");
  rungLabel.className = "dial-rung";
  rungLabel.textContent = RUNG_LABELS[block.rung];
  head.append(mark, tip, rungLabel);

  // The live-helper count, shown only while it is a fact — a fixed
  // mark at the head's own station, not a rung any click reveals.
  if (block.helpers) {
    const helpers = document.createElement("span");
    helpers.className = "dial-helpers";
    helpers.textContent = block.helpers + (block.helpers === 1 ? " helper" : " helpers");
    head.appendChild(helpers);
  }

  const body = document.createElement("div");
  body.className = "dial-body";
  for (const entry of block.entries) {
    const item = renderDialEntry(entry, block.rung);
    if (item) body.appendChild(item);
  }
  if (block.rung === 2 && block.notes.length) {
    body.appendChild(renderDialNotes(block.notes));
  }

  el.append(head, body);
  // The body carries no chrome of its own when it has nothing to show
  // — a rung-0 gap of pure calls, or a dial opened by a lone note.
  el.classList.toggle("empty-body", body.childElementCount === 0);
  return el;
}

// One entry's contribution at the given rung: the step intent from
// rung 1, the script and process cards from rung 2. A warn/bad note
// entry shows at every rung — it is a fact of the exchange, not
// process to be dialed for. Returns null when the entry has nothing to
// show at this rung (e.g. any call at rung 0) so the body stays free
// of empty rows.
function renderDialEntry(entry, rung) {
  if (entry.kind === "note") return noteLine(entry);
  if (entry.kind === "helper") return rung >= 2 ? helperLine(entry) : null;
  const children = [
    rung >= 1 && entry.intent ? entryHeading(entry) : null,
    rung >= 2 ? entryScript(entry) : null,
    ...(rung >= 2 ? entry.cards.map(renderMarkCard) : []),
  ].filter(Boolean);
  if (!children.length) return null;
  const item = document.createElement("div");
  item.className = "dial-entry";
  item.append(...children);
  return item;
}

function entryHeading(entry) {
  const heading = document.createElement("div");
  heading.className = "dial-entry-head" + (entry.kind === "harness" && entry.failed ? " failed" : "");
  heading.textContent = entry.intent;
  return heading;
}

// The script an entry ran, or null for one that carries none.
function entryScript(entry) {
  const script = entry.kind === "harness" ? entry.detail : entry.script;
  if (!script) return null;
  const pre = document.createElement("pre");
  pre.className = "dial-script";
  pre.textContent = script;
  return pre;
}

function renderDialNotes(notes) {
  const box = document.createElement("div");
  box.className = "dial-notes";
  box.append(...notes.map(noteLine));
  return box;
}

function noteLine(note) {
  const line = document.createElement("div");
  line.className = "dial-note" + (note.cls ? " " + note.cls : "");
  line.textContent = note.text;
  return line;
}

function helperLine(entry) {
  const line = document.createElement("div");
  line.className = "dial-note" + (entry.ok ? "" : " bad");
  line.textContent = entry.text;
  return line;
}

export function dialAddCall(p) {
  const dial = ensureDial();
  dial.entries.push({ kind: "call", intent: p.summary || p.tool, script: p.cmd, cards: [] });
  bump(dial);
}

export function dialAddHarness(p) {
  const dial = ensureDial();
  dial.entries.push({
    kind: "harness",
    intent: p.verb + (p.subject ? " " + p.subject : ""),
    detail: p.payload,
    failed: Boolean(p.failed),
    cards: [],
  });
  bump(dial);
}

// A process card dresses work the dial already narrates — an exec's io,
// a worker's settling, housekeeping — so it folds into the entry that
// did the work and shows only at the deepest rung, beside the scripts.
// Like muted telemetry it has no trigger of its own: with no dial open
// in this gap it is dropped, not surfaced.
export function dialAddProcessCard(marks) {
  if (!marks.length || !openDial) return;
  const last = openDial.entries[openDial.entries.length - 1];
  if (last && last.cards) last.cards.push(marks);
  else openDial.entries.push({ kind: "card", intent: null, cards: [marks] });
  bump(openDial);
}

// Muted telemetry (usage, steps, stop reasons) has no trigger of its own
// — it only folds into a dial a call/harness/card already opened in this
// gap, and is dropped when there is none, since it has nothing to say on
// its own; it shows only at the last rung. A warn/bad note (an error, a
// provider error) is never swallowed: it joins the entries in temporal
// order, visible at every rung, and opens a dial when none is, since a
// failed exchange must never look like a quiet one.
// The live-helper count: a fact of the whole exchange, not one call, so
// it lands on the dial itself rather than in its list of entries — a
// fixed station `renderDialBlock` reads straight off the block.
export function dialAddHelpers(live) {
  const dial = ensureDial();
  dial.helpers = live;
  bump(dial);
}

// A helper's settling: process, like a `ProcessCard`, so it shows only
// at the dial's deepest rung, beside the scripts it kept beneath.
export function dialAddHelperDone(p) {
  const dial = ensureDial();
  dial.entries.push({
    kind: "helper",
    ok: Boolean(p.ok),
    text: "helper \"" + p.name + "\" " + (p.ok ? "finished" : "did not finish")
      + " (" + p.elapsed_secs.toFixed(1) + "s)",
  });
  bump(dial);
}

export function dialAddNote(text, cls) {
  const urgent = cls === "warn" || cls === "bad";
  const dial = urgent ? ensureDial() : openDial;
  if (!dial) return;
  if (urgent) dial.entries.push({ kind: "note", text, cls });
  else dial.notes.push({ text, cls: cls || "" });
  bump(dial);
}
