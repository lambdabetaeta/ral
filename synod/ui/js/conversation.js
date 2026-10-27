import { $, show } from "./dom.js";
import { state, confirmDialog } from "./core.js";
import { startConversation, sendMessage, openUrl, onConversationEvent } from "./ipc.js";
import { setFacts, setStatus } from "./status.js";
import {
  addUserMessage,
  addSystemMessage,
  resetDocument,
  clearOpenDial,
  closeStreamingProse,
  setThinking,
} from "./chat-document.js";
import { showReport, resetReport } from "./change-report.js";
import { onOpening, onSynodEvent } from "./projector.js";

/** Parent path in faint ink, basename in full; the bdi keeps the rtl lead's slashes in order. */
function renderFolder(label) {
  const path = label.replace(/\/+$/, "");
  const cut = path.lastIndexOf("/") + 1;
  const parts = [];
  if (cut > 0) {
    const lead = document.createElement("span");
    lead.className = "lead";
    const text = document.createElement("bdi");
    text.dir = "ltr";
    text.textContent = path.slice(0, cut);
    lead.append(text);
    parts.push(lead);
  }
  const base = document.createElement("span");
  base.className = "base";
  base.textContent = path.slice(cut) || label;
  parts.push(base);
  const el = $("convo-folder");
  el.replaceChildren(...parts);
  el.title = label;
}

function resetTranscript(folder) {
  state.folder = folder;
  renderFolder(folder.label);
  setFacts(null);
  // A restart must not inherit the superseded conversation's outstanding state.
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

/** @typedef {{ attempt: string, phase: "starting" } | { attempt: string, phase: "ended" } | { attempt: string, phase: "accepted" | "ready", exchange: "idle" | "busy" }} Attempt */
/** @typedef {{ attempt: string, type: "accepted" | "opening" | "submit" | "exchange_done" | "send_failed" | "ended" }} Action */
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
    await startConversation(folder.path, choice, attempt);
    // Accepted messages queue until boot; acceptance is not readiness.
    transition({ attempt, type: "accepted" });
  } catch (err) {
    if (transition({ attempt, type: "ended" })) {
      settleExchange();
      addSystemMessage(String(err));
    }
  }
}

function settleExchange() {
  closeStreamingProse();
  clearOpenDial();
  setThinking(false);
  setStatus("");
}

// Composer enablement must not overwrite the shell's current status narration.
function refreshComposer() {
  /** @type {HTMLTextAreaElement} */ ($("message")).disabled = !canSend();
  /** @type {HTMLButtonElement} */ ($("send")).disabled = !canSend();
}

// Send raw links to the shell for scheme checks; never navigate the conversation webview.
$("transcript").addEventListener("click", (e) => {
  if (!(e.target instanceof Element)) return;
  const anchor = e.target.closest("a[href]");
  if (!anchor) return;
  e.preventDefault();
  const href = anchor.getAttribute("href");
  if (href) openUrl(href).catch((err) => addSystemMessage(String(err)));
});

onConversationEvent(({ attempt, update }) => {
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
        showReport(update.report);
      }
      break;
    case "ended":
      if (!transition({ attempt, type: "ended" })) return;
      settleExchange();
      if (!update.explained) addSystemMessage("The assistant stopped unexpectedly. Choose Start again to continue.");
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
  sendMessage(attempt, text).catch((err) => {
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
