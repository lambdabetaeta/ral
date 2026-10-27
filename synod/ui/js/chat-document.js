import { renderMarkCard } from "./card-renderer.js";
import { highlightRal } from "./ral-highlight.js";

// Blocks are transcript data; only the projector reconciles them with the DOM.
export let blocks = [];

let changed = () => {};

/** @param {() => void} fn */
export function onChange(fn) {
  changed = fn;
}

// Prose and dial streams are mutually exclusive; closing leaves their blocks in the document.
/** @type {{ kind: "assistant", raw: string, open: boolean, rev: number } | null} */
export let streamingProse = null;
let openDial = null;

export function resetDocument() {
  blocks = [];
  streamingProse = null;
  openDial = null;
  changed();
}

export function clearOpenDial() {
  openDial = null;
}

export function openStreamingProse(text) {
  streamingProse = pushBlock({ kind: "assistant", raw: text, open: true });
}

// Every mutation bumps rev and requests projection; unchanged revisions retain their elements.
function pushBlock(block) {
  block.rev = 0;
  blocks.push(block);
  changed();
  return block;
}

export function bump(block) {
  block.rev += 1;
  changed();
}

export function closeStreamingProse() {
  if (!streamingProse) return;
  streamingProse.open = false;
  bump(streamingProse);
  streamingProse = null;
}

export function addUserMessage(text) {
  pushBlock({ kind: "user", text });
}

export function addSystemMessage(text) {
  pushBlock({ kind: "system", text });
}

// Surfaced cards close both streams, preserving temporal order.
export function addCardBlock(marks) {
  if (!marks.length) return; // an empty card would render as bare chrome
  closeStreamingProse();
  openDial = null;
  pushBlock({ kind: "card", marks, rung: 1 });
}

const RUNG_LABELS = ["", "all steps", "all scripts"];

// A new dial enters with its first entry, never as an empty shell; opening ends prose.
function ensureDial() {
  if (openDial) return openDial;
  closeStreamingProse();
  openDial = { kind: "dial", entries: [], notes: [], rung: 0, helpers: 0, rev: 0 };
  blocks.push(openDial);
  return openDial;
}

export function rotateRung(block) {
  block.rung = (block.rung + 1) % 3;
  bump(block);
}

// The tip names the latest step or warning, so retries and failures stay visible when collapsed.
function dialHeaderText(dial) {
  const latest = dial.entries.findLast((e) => e.intent || e.text);
  return latest ? latest.intent || latest.text : "";
}

export function renderDialBlock(block) {
  const el = document.createElement("div");
  el.className = "dial";
  el.dataset.rung = String(block.rung);

  const head = document.createElement("button");
  head.type = "button";
  head.className = "dial-head";
  head.setAttribute("aria-expanded", String(block.rung > 0));
  head.addEventListener("click", () => rotateRung(block));

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
  el.classList.toggle("empty-body", body.childElementCount === 0);
  return el;
}

// Warn/bad entries show at every rung; calls start at 1, scripts and process cards at 2.
function renderDialEntry(entry, rung) {
  if (entry.kind === "note") return noteLine(entry);
  if (entry.kind === "readout") return readoutBlock(entry);
  if (entry.kind === "helper") return rung >= 2 ? helperLine(entry) : null;
  const children = [
    rung >= 1 && entry.intent ? entryHeading(entry) : null,
    rung >= 2 ? entryScript(entry) : null,
    ...(rung >= 2 ? entry.cards.map((marks) => renderMarkCard(marks)) : []),
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

function entryScript(entry) {
  const script = entry.kind === "harness" ? entry.detail : entry.script;
  if (!script) return null;
  const pre = document.createElement("pre");
  pre.className = "dial-script";
  pre.textContent = script;
  highlightRal(pre);
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

function readoutBlock(r) {
  const box = document.createElement("div");
  box.className = "dial-entry";
  const line = (text, cls) => noteLine({ text, cls });
  const grid = document.createElement("div");
  grid.className = "fields";
  for (const { label, value } of r.fields) {
    const l = document.createElement("div");
    l.className = "f-label";
    l.textContent = label;
    const v = document.createElement("div");
    v.className = "f-value";
    v.textContent = value;
    grid.append(l, v);
  }
  box.append(line(r.headline, r.severity), ...(r.fields.length ? [grid] : []),
    ...r.hints.map((h) => line("↳ " + h, "muted")));
  return box;
}

export function dialAddReadout(p) {
  const dial = ensureDial();
  dial.entries.push({ kind: "readout", ...p });
  bump(dial);
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

// Process cards fold into an existing dial at the deepest rung; they never open one.
export function dialAddProcessCard(marks) {
  if (!marks.length || !openDial) return;
  const last = openDial.entries[openDial.entries.length - 1];
  if (last && last.cards) last.cards.push(marks);
  else openDial.entries.push({ kind: "card", intent: null, cards: [marks] });
  bump(openDial);
}

// Busy helpers belong to the exchange.
export function dialAddHelpers(busy) {
  const dial = ensureDial();
  dial.helpers = busy;
  bump(dial);
}

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

// Telemetry cannot open a dial; warn/bad notes always do.
export function dialAddNote(text, cls) {
  const urgent = cls === "warn" || cls === "bad";
  const dial = urgent ? ensureDial() : openDial;
  if (!dial) return;
  if (urgent) dial.entries.push({ kind: "note", text, cls });
  else dial.notes.push({ text, cls: cls || "" });
  bump(dial);
}
