import { renderAssistantMarkdown } from "./math.js";
import {
  blocks,
  streamingProse,
  bump,
  renderDialBlock,
  clearOpenDial,
  openStreamingProse,
  addSystemMessage,
  addNoticeMessage,
  dialAddNote,
  dialAddCall,
  dialAddHarness,
  addCardBlock,
  dialAddProcessCard,
  dialAddHelpers,
  dialAddHelperDone,
} from "./chat-document.js";
import { renderMarkCard } from "./card-renderer.js";
import { transcriptEl, pinTranscript } from "./core.js";
import { setFacts, setStatus } from "./conversation.js";

// ---- The projector ----------------------------------------------------

// Per block: the element last built for it and the revision it shows.
// An untouched block keeps its element across renders — same node, same
// in-flight animation. A block whose rev moved is rebuilt whole; the
// rebuild is a replacement, not an arrival, and wears `settled` so the
// entrance animation stays with first appearances.
const memo = new WeakMap();

function projectBlock(block) {
  const known = memo.get(block);
  if (known && known.rev === block.rev) return known.el;
  const el = renderBlock(block);
  // renderBlock's switch covers every kind chat-document.js ever creates,
  // so it always returns — TS just can't see that without a default arm.
  if (known) /** @type {HTMLElement} */ (el).classList.add("settled");
  memo.set(block, { el, rev: block.rev });
  return el;
}

// An empty document projects to the welcome line, never to a bare
// column. One element for the app's lifetime, so reconciliation holds
// it in place across empty renders.
const welcomeEl = (() => {
  const p = document.createElement("p");
  p.className = "welcome";
  p.textContent = "Say what you'd like done, in your own words — for example, "
    + '"file every invoice under the month it was sent." You can keep talking; the '
    + "assistant remembers this conversation until you start again.";
  return p;
})();

// Reconcile the transcript's children, in order, against the projected
// list: a child no longer wanted anywhere (a stale revision, the welcome
// line) is removed on sight, a wanted element missing at its position is
// inserted, and trailing leftovers are trimmed. A surviving node is
// never detached — detaching would restart its animation.
// Coalesced onto rAF: many `bump`s in one frame (a token burst) collapse
// to one reconciliation, and a render requested mid-frame still lands on
// the next one rather than being lost.
let renderScheduled = false;
export function render() {
  if (renderScheduled) return;
  renderScheduled = true;
  requestAnimationFrame(renderNow);
}

function renderNow() {
  renderScheduled = false;
  const desired = blocks.length ? blocks.map(projectBlock) : [welcomeEl];
  const wanted = new Set(desired);
  desired.forEach((el, i) => {
    while (transcriptEl.children[i] && transcriptEl.children[i] !== el && !wanted.has(transcriptEl.children[i])) {
      transcriptEl.children[i].remove();
    }
    if (transcriptEl.children[i] !== el) {
      transcriptEl.insertBefore(el, transcriptEl.children[i] || null);
    }
  });
  while (transcriptEl.children.length > desired.length) {
    // The loop guard guarantees a last child exists.
    /** @type {Element} */ (transcriptEl.lastElementChild).remove();
  }
  pinTranscript();
}

function renderBlock(block) {
  switch (block.kind) {
    case "user": {
      const { msg, bubble } = messageShell("msg user");
      bubble.textContent = block.text;
      return msg;
    }
    // While tokens are still landing, plain text: a markdown re-parse of
    // the whole accumulated message on every token is quadratic in the
    // message length (measured: 3.3s of main-thread time by 8,000
    // tokens). `block.plain` drops only at a streaming flush boundary or
    // when the bubble's stream closes, so the expensive parse runs a
    // handful of times per message rather than once per token.
    case "assistant": {
      const { msg, bubble } = messageShell("msg assistant");
      if (block.plain) {
        bubble.textContent = block.raw;
      } else {
        bubble.innerHTML = renderAssistantMarkdown(block.raw);
      }
      return msg;
    }
    case "system": {
      const { msg, bubble } = messageShell("msg system");
      bubble.textContent = block.text;
      return msg;
    }
    case "notice": {
      const { msg, bubble } = messageShell("msg system info");
      bubble.textContent = block.text;
      return msg;
    }
    // A surfaced card wears the `msg` wrap for its transcript standing
    // — width, entrance, left alignment — but no bubble: the mark-card
    // is its own chrome.
    case "card": {
      const msg = document.createElement("div");
      msg.className = "msg";
      msg.appendChild(renderMarkCard(block.marks));
      return msg;
    }
    case "dial":
      return renderDialBlock(block);
  }
}

function messageShell(cls) {
  const msg = document.createElement("div");
  msg.className = cls;
  const bubble = document.createElement("div");
  bubble.className = "bubble";
  msg.appendChild(bubble);
  return { msg, bubble };
}

// Not `Intl`'s compact notation: it renders a capital "1.5K", and the
// status line has always read in lowercase.
function formatK(n) {
  return n >= 1000 ? (n / 1000).toFixed(1) + "k" : String(n);
}

function formatUsage(p) {
  let s = formatK(p.input ?? 0) + " in · " + formatK(p.output ?? 0) + " out";
  if (!p.unmetered && p.dollars !== undefined && p.dollars !== null) {
    s += " · $" + Number(p.dollars).toFixed(2);
  }
  return s;
}

/** @param {import("./bindings/Opening.ts").Opening} p */
export function onOpening(p) {
  setFacts(p);
  if (p.folder_line) addNoticeMessage(p.folder_line);
}

/** @param {import("./bindings/SynodEvent.ts").SynodEvent} p */
export function onSynodEvent(p) {
  switch (p.type) {
    case "token":
      // A whitespace-only exhale before any prose opens no bubble and
      // closes no dial — it would leave an empty pill in the transcript
      // and split the gap's work across two dials for nothing.
      if (!streamingProse && !p.text.trim()) break;
      if (streamingProse) {
        streamingProse.raw += p.text;
        // A token past a boundary flush resumes in plain text — the
        // markdown re-parse that flush just paid for stays paid for
        // until the next boundary, not redone on every token after it.
        streamingProse.plain = true;
        bump(streamingProse);
      } else {
        clearOpenDial();
        openStreamingProse(p.text);
      }
      break;
    // The streaming flush boundary: a step of the reply has settled, so
    // this is a cheap moment to pay the one markdown re-parse the plain
    // text has been deferring. More tokens past it resume in plain text.
    case "boundary":
      if (streamingProse) {
        streamingProse.plain = false;
        bump(streamingProse);
      }
      break;
    // The session's own state — the shell's before there is an agent, the
    // agent's after — and the status bar's only writer besides "Working…", so
    // the label never flickers back to a coarser word as tokens arrive;
    // `ready` carries no spinner, which is what an empty bar means here.
    case "state":
      setStatus(p.pending ? (p.label ? p.label.charAt(0).toUpperCase() + p.label.slice(1) : p.label) + "…" : "");
      break;
    case "turn":
      dialAddNote("[turn " + p.id + "]", "muted");
      break;
    case "tool_call":
      dialAddCall(p);
      break;
    case "harness_call":
      dialAddHarness(p);
      break;
    case "card":
      addCardBlock(p.marks || []);
      break;
    case "process_card":
      dialAddProcessCard(p.marks || []);
      break;
    case "helpers":
      dialAddHelpers(p.live);
      break;
    case "helper_done":
      dialAddHelperDone(p);
      break;
    case "usage":
      dialAddNote(formatUsage(p), "muted");
      break;
    case "stop_reason":
      dialAddNote("[stop: " + p.reason + "]", "muted");
      break;
    case "error":
      dialAddNote("error: " + p.message, "bad");
      break;
    case "provider_error":
      dialAddNote(p.text, p.severity);
      break;
    // Always "warn", never "bad", whatever the cause: the exchange survived
    // it and the partial reply above the note stands. `sink.rs` pins this
    // severity itself, rather than deriving it here from the record.
    case "stalled":
      dialAddNote(p.text, p.severity);
      break;
    case "failure":
      addSystemMessage(p.message);
      break;
    // Unreachable, and saying so in the type system is the point: `p` narrows
    // to `never` only when every variant above is handled, so a new Rust
    // variant the window forgets becomes a build failure rather than an event
    // that silently does nothing.  This is the half `deno check` cannot see
    // from the `case` labels alone.
    default: {
      /** @type {never} */
      const _unhandled = p;
      break;
    }
  }
}
