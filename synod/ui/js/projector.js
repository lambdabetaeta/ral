import { growProse } from "./prose.js";
import {
  blocks,
  onChange,
  streamingProse,
  bump,
  renderDialBlock,
  clearOpenDial,
  openStreamingProse,
  addSystemMessage,
  dialAddNote,
  dialAddReadout,
  dialAddCall,
  dialAddHarness,
  addCardBlock,
  rotateRung,
  thinking,
  setThinking,
  dialAddProcessCard,
  dialAddHelpers,
  dialAddHelperDone,
} from "./chat-document.js";
import { renderMarkCard } from "./card-renderer.js";
import { armillaryEl, startArmillary, stopArmillary } from "./armillary.js";
import { transcriptEl, pinTranscript } from "./core.js";
import { setFacts, setStatus } from "./status.js";

// Unchanged revisions retain their nodes and animations; replacements are marked settled.
const memo = new WeakMap();

function projectBlock(block) {
  const known = memo.get(block);
  if (known && known.rev === block.rev) return known.el;
  // Update prose in place; settled markdown and typeset math must not be rebuilt.
  if (block.kind === "assistant" && known) {
    growProse(/** @type {HTMLElement} */ (known.el.firstElementChild), block);
    memo.set(block, { el: known.el, rev: block.rev });
    return known.el;
  }
  const el = renderBlock(block);
  // renderBlock is exhaustive; TypeScript cannot infer that without its default arm.
  if (known) /** @type {HTMLElement} */ (el).classList.add("settled");
  memo.set(block, { el, rev: block.rev });
  return el;
}

// Reuse one welcome element across empty renders.
const welcomeEl = (() => {
  const p = document.createElement("p");
  p.className = "welcome";
  p.textContent = "Say what you'd like done, in your own words — for example, "
    + '"file every invoice under the month it was sent." You can keep talking; the '
    + "assistant remembers this conversation until you start again.";
  return p;
})();

// Reconcile without detaching surviving nodes, which would restart animations; coalesce on rAF.
let renderScheduled = false;
export function render() {
  if (renderScheduled) return;
  renderScheduled = true;
  requestAnimationFrame(renderNow);
}

onChange(render);

let armillaryShown = false;

function renderNow() {
  renderScheduled = false;
  const desired = blocks.length ? blocks.map(projectBlock) : [welcomeEl];
  const showArmillary = thinking && blocks.length > 0;
  if (showArmillary) desired.push(armillaryEl);
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
  if (showArmillary !== armillaryShown) {
    (showArmillary ? startArmillary : stopArmillary)();
    armillaryShown = showArmillary;
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
    case "assistant": {
      const { msg, bubble } = messageShell("msg assistant");
      growProse(bubble, block);
      return msg;
    }
    case "system": {
      const { msg, bubble } = messageShell("msg system");
      bubble.textContent = block.text;
      return msg;
    }
    case "card": {
      const msg = document.createElement("div");
      msg.className = "msg";
      msg.appendChild(renderMarkCard(block.marks, block.rung, () => rotateRung(block)));
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

// Intl compact notation gives 1.5K; the status line uses lowercase.
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

/** @param {import("./bindings/AgentStateDto.ts").AgentStateDto} state */
function stateText(state) {
  switch (state) {
    case "ready":
      return "";
    case "awaiting_model":
      return "Awaiting model…";
    case "evaluating":
      return "Evaluating…";
    case "evicting":
      return "Evicting…";
    case "waiting_on_agents":
      return "Waiting on agents…";
    default: {
      /** @type {never} */
      const unhandled = state;
      return unhandled;
    }
  }
}

/** @param {import("./bindings/Preparation.ts").Preparation} stage */
function preparationText(stage) {
  switch (stage.kind) {
    case "finishing_previous":
      return "Finishing the previous session…";
    case "starting":
      return "Starting…";
    case "reading_folder":
      return `Reading the folder: ${stage.files} files seen so far…`;
    default: {
      /** @type {never} */
      const unhandled = stage;
      return unhandled;
    }
  }
}

/** @param {import("./bindings/Opening.ts").Opening} p */
export function onOpening(p) {
  setFacts(p);
}

/** @param {import("./bindings/SynodEvent.ts").SynodEvent} p */
export function onSynodEvent(p) {
  switch (p.type) {
    case "token":
      // Whitespace before prose must not open an empty bubble or split the dial.
      if (!streamingProse && !p.text.trim()) break;
      setThinking(false);
      if (streamingProse) {
        streamingProse.raw += p.text;
        streamingProse.open = true;
        bump(streamingProse);
      } else {
        clearOpenDial();
        openStreamingProse(p.text);
      }
      break;
    case "boundary":
      if (streamingProse) {
        streamingProse.open = false;
        bump(streamingProse);
      }
      break;
    case "state":
      setThinking(p.state === "awaiting_model");
      setStatus(stateText(p.state));
      break;
    case "preparing":
      setThinking(false);
      setStatus(preparationText(p.stage));
      break;
    case "tool_call":
      setThinking(false);
      dialAddCall(p);
      break;
    case "harness_call":
      setThinking(false);
      dialAddHarness(p);
      break;
    case "card":
      setThinking(false);
      addCardBlock(p.marks || []);
      break;
    case "process_card":
      setThinking(false);
      dialAddProcessCard(p.marks || []);
      break;
    case "helpers":
      dialAddHelpers(p.busy);
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
      setThinking(false);
      dialAddNote("error: " + p.message, "bad");
      break;
    case "provider_error":
      setThinking(false);
      dialAddReadout(p);
      break;
    // Stalled is always Warn: the exchange survived and its partial reply stands.
    case "stalled":
      setThinking(false);
      dialAddReadout(p);
      break;
    case "failure":
      setThinking(false);
      addSystemMessage(p.message);
      break;
    default: {
      /** @type {never} */
      const _unhandled = p;
      break;
    }
  }
}
