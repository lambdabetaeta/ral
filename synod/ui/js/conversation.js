import { $, invoke, listen, state, show, confirmDialog } from "./core.js";
import {
  addUserMessage,
  addSystemMessage,
  resetDocument,
  clearOpenDial,
  closeStreamingProse,
} from "./chat-document.js";
import { refreshReport, resetReport } from "./change-report.js";
import { onOpening, onSynodEvent } from "./projector.js";

// ---- Conversation screen --------------------------------------------

// The standing facts of a conversation, written only from the opening
// the backend narrates — the model and effort actually in force, which
// is not always what the start screen asked for (a model that takes no
// reasoning control is sent none). `null` clears them, for the gap
// between a restart and its opening.
export function setFacts(opening) {
  const model = opening ? opening.model || "" : "";
  const label = opening ? opening.label || "" : "";
  $("convo-model").textContent = model;
  $("convo-effort").textContent = opening ? opening.effort || "" : "";
  $("convo-model").title = label ? label + " · " + model : "";
  $("convo-facts").classList.toggle("ready", Boolean(opening));
}

function resetTranscript(folder) {
  state.folder = folder;
  $("convo-folder").textContent = folder.label;
  setFacts(null);
  // Nothing is outstanding until the shell says so; a restart must not
  // inherit the superseded conversation's last word.
  setStatus("");
  resetDocument();
  resetReport();
  show("conversation");
}

export function enterConversation(folder, choice) {
  state.choice = choice;
  resetTranscript(folder);
  startAssistant(folder, choice);
}

/**
 * @typedef {{ attempt: string, phase: "starting" }
 *   | { attempt: string, phase: "ended" }
 *   | { attempt: string, phase: "accepted" | "ready", exchange: "idle" | "busy" }} Attempt
 * @typedef {{ attempt: string, type: "accepted" | "opening" | "submit" | "exchange_done" | "send_failed" | "ended" }} Action
 */
/** @type {Attempt} */
let assistant = { attempt: "", phase: "ended" };

// Null means ignored; terminal attempts are absorbing.
/** @param {Attempt} current @param {Action} action @returns {Attempt | null} */
function advance(current, action) {
  if (current.attempt !== action.attempt || current.phase === "ended") return null;
  const attempt = current.attempt;
  switch (action.type) {
    case "accepted":
      return current.phase === "starting" ? { attempt, phase: "accepted", exchange: "idle" } : null;
    case "opening":
      if (current.phase === "ready") return null;
      return { attempt, phase: "ready", exchange: current.phase === "accepted" ? current.exchange : "idle" };
    case "submit":
      return current.phase !== "starting" && current.exchange === "idle" ? { ...current, exchange: "busy" } : null;
    case "exchange_done":
    case "send_failed":
      return current.phase !== "starting" && current.exchange === "busy" ? { ...current, exchange: "idle" } : null;
    case "ended":
      return { attempt, phase: "ended" };
    default: {
      /** @type {never} */
      const unhandled = action.type;
      return unhandled;
    }
  }
}

/** @param {Action} action */
function transition(action) {
  const next = advance(assistant, action);
  if (!next) return false;
  assistant = next;
  refreshComposer();
  return true;
}

function canSend() {
  return advance(assistant, { attempt: assistant.attempt, type: "submit" }) !== null;
}

async function startAssistant(folder, choice) {
  const attempt = crypto.randomUUID();
  assistant = { attempt, phase: "starting" };
  refreshComposer();
  try {
    await invoke("start_conversation", { folder: folder.path, choice, attempt });
    // Accepted messages queue until boot; acceptance is not readiness.
    transition({ attempt, type: "accepted" });
  } catch (err) {
    if (transition({ attempt, type: "ended" })) {
      settleExchange();
      addSystemMessage(String(err));
    }
  }
}

export function setStatus(text) {
  const el = $("convo-status");
  el.textContent = "";
  if (!text) return;
  const spinner = document.createElement("span");
  spinner.className = "spinner";
  spinner.setAttribute("aria-hidden", "true");
  el.append(spinner, text);
}

function settleExchange() {
  closeStreamingProse();
  clearOpenDial();
  setStatus("");
}

// The composer's enabled state, with no word about the status bar: a start
// is narrated by the shell, and that sentence must survive the input
// becoming usable again.
function refreshComposer() {
  /** @type {HTMLTextAreaElement} */ ($("message")).disabled = !canSend();
  /** @type {HTMLButtonElement} */ ($("send")).disabled = !canSend();
}

// A link the assistant wrote is opened in the user's own browser, never
// followed inside the window — the window's one document is this
// conversation, and navigating it away would throw the conversation out.
// DOMPurify lets a bare name or a share path through as a relative link,
// and the system would run whatever such a link named, so the raw href goes
// to `open_url`, which opens only web and email links and answers anything
// else with a sentence of its own. That check is the boundary; nothing here
// second-guesses it.
$("transcript").addEventListener("click", (e) => {
  // The click target of a listener on an element is always an Element,
  // never bare text or the document itself.
  if (!(e.target instanceof Element)) return;
  const anchor = e.target.closest("a[href]");
  if (!anchor) return;
  e.preventDefault();
  const href = anchor.getAttribute("href");
  if (href) invoke("open_url", { url: href }).catch((err) => addSystemMessage(String(err)));
});

listen("conversation-event", (event) => {
  const { attempt, update } = /** @type {import("./bindings/ConversationEvent.ts").ConversationEvent} */ (event.payload);
  if (assistant.attempt !== attempt || assistant.phase === "ended") return;
  switch (update.type) {
    case "opening":
      if (transition({ attempt, type: "opening" })) onOpening(update.opening);
      break;
    case "event":
      onSynodEvent(update.event);
      break;
    case "exchange_done":
      if (transition({ attempt, type: "exchange_done" })) {
        settleExchange();
        refreshReport();
      }
      break;
    case "ended":
      if (!transition({ attempt, type: "ended" })) return;
      settleExchange();
      if (!update.failed.explained) addSystemMessage("The assistant stopped unexpectedly. Choose Start again to continue.");
      break;
    default: {
      /** @type {never} */
      const unhandled = update;
      return unhandled;
    }
  }
});

function submit() {
  const field = /** @type {HTMLTextAreaElement} */ ($("message"));
  const text = field.value.trim();
  const attempt = assistant.attempt;
  if (!text || !transition({ attempt, type: "submit" })) return;
  addUserMessage(text);
  field.value = "";
  setStatus("Working…");
  invoke("send_message", { attempt, message: text }).catch((err) => {
    if (!transition({ attempt, type: "send_failed" })) return;
    addSystemMessage(String(err));
    settleExchange();
  });
}

$("send").addEventListener("click", submit);
$("message").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    submit();
  }
});

$("start-again").addEventListener("click", async () => {
  const button = /** @type {HTMLButtonElement} */ ($("start-again"));
  if (button.disabled) return;
  button.disabled = true;
  try {
    if (!await confirmDialog("This begins a fresh conversation. Everything already " +
      "changed in your folder stays exactly as it is.", { title: "Start again?", kind: "warning" })) return;
    resetTranscript(state.folder);
    await startAssistant(state.folder, state.choice);
  } finally {
    button.disabled = false;
  }
});
