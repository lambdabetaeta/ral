import { $, show, screens } from "./dom.js";
import { advance } from "./attempt.js";
import { withdrawals } from "./withdrawals.js";
import { state, confirmDialog } from "./core.js";
import { startConversation, sendMessage, stopJob, recallQueued, openUrl, onConversationEvent } from "./ipc.js";
import { setFacts, setStatus } from "./status.js";
import {
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
  renderQueue([]);
  show("conversation");
}

export function enterConversation(folder, choice) {
  state.choice = choice;
  resetTranscript(folder);
  startAssistant(folder, choice);
}

/** @typedef {import("./attempt.js").Attempt} Attempt */
/** @typedef {import("./attempt.js").Action} Action */
/** @type {Attempt} */
let assistant = { attempt: "", phase: "ended" };
/** @type {string[]} */
let queued = [];
let withdraw = withdrawalQueue(assistant.attempt);

/** @param {string} attempt */
function withdrawalQueue(attempt) {
  return withdrawals(
    () => assistant.attempt === attempt,
    restoreToComposer,
    (error) => addSystemMessage(String(error)),
  );
}

/** @param {Action} action */
function transition(action) {
  const next = advance(assistant, action);
  if (!next) return false;
  assistant = next;
  refreshComposer();
  return true;
}

function canCompose() {
  return assistant.phase === "accepted" || assistant.phase === "ready";
}

function busy() {
  return (assistant.phase === "accepted" || assistant.phase === "ready") && assistant.job !== null;
}

async function startAssistant(folder, choice) {
  const attempt = crypto.randomUUID();
  assistant = { attempt, phase: "starting" };
  withdraw = withdrawalQueue(attempt);
  refreshComposer();
  try {
    await startConversation(folder.path, choice, attempt);
    // Accepted messages queue until boot; acceptance is not readiness.
    transition({ attempt, type: "accepted" });
  } catch (err) {
    if (transition({ attempt, type: "ended" })) {
      settleJob();
      addSystemMessage(String(err));
    }
  }
}

function settleJob() {
  closeStreamingProse();
  clearOpenDial();
  setThinking(false);
  setStatus("");
}

// Composer enablement must not overwrite the shell's current status narration.
function refreshComposer() {
  const field = /** @type {HTMLTextAreaElement} */ ($("message"));
  field.disabled = !canCompose();
  field.placeholder = busy() ? "Queue a follow-up…" : "What would you like done?";
  /** @type {HTMLButtonElement} */ ($("send")).disabled = !canCompose();
  $("stop").hidden = !busy();
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
    case "job_began":
      if (transition({ attempt, type: "job_began", job: update.job })) setStatus("Working…");
      break;
    case "queue":
      renderQueue(update.messages);
      break;
    case "job_done":
      if (transition({ attempt, type: "job_done", job: update.job })) {
        settleJob();
        if (update.stopped) addSystemMessage("Stopped.");
        showReport(update.report);
      }
      break;
    case "ended":
      if (!transition({ attempt, type: "ended" })) return;
      settleJob();
      restoreToComposer(queued);
      renderQueue([]);
      if (!update.explained) addSystemMessage("The assistant stopped unexpectedly. Choose Start again to continue.");
      break;
    default: {
      /** @type {never} */
      const unhandled = update;
      return unhandled;
    }
  }
});

async function submit() {
  const field = /** @type {HTMLTextAreaElement} */ ($("message"));
  const text = field.value.trim();
  const attempt = assistant.attempt;
  if (!text || !canCompose()) return;
  field.value = "";
  try {
    await sendMessage(attempt, text);
  } catch (err) {
    if (assistant.attempt !== attempt) return;
    restoreToComposer([text]);
    addSystemMessage(String(err));
  }
}

/** @param {string[]} prompts */
function restoreToComposer(prompts) {
  const field = /** @type {HTMLTextAreaElement} */ ($("message"));
  field.value = [...prompts, field.value].filter(s => s.trim()).join("\n\n");
  field.focus();
}

function stop() {
  if (assistant.phase !== "accepted" && assistant.phase !== "ready") return;
  const { attempt, job } = assistant;
  if (job === null) return;
  return withdraw(() => stopJob(attempt, job));
}

function recall() {
  const attempt = assistant.attempt;
  return withdraw(() => recallQueued(attempt));
}

/** @param {string[]} messages */
function renderQueue(messages) {
  queued = messages;
  const strip = $("queued");
  strip.replaceChildren();
  strip.hidden = messages.length === 0;
  for (const message of messages) {
    const text = document.createElement("div");
    text.textContent = message;
    strip.append(text);
  }
  if (messages.length) {
    const edit = document.createElement("button");
    edit.type = "button";
    edit.className = "btn-quiet";
    edit.textContent = "Edit";
    edit.addEventListener("click", recall);
    strip.append(edit);
  }
}

$("stop").addEventListener("click", stop);
$("send").addEventListener("click", submit);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && screens.conversation.classList.contains("active") && busy()) {
    e.preventDefault();
    stop();
  }
});
$("message").addEventListener("keydown", (e) => {
  if (e.key === "ArrowUp" && !/** @type {HTMLTextAreaElement} */ ($("message")).value && queued.length) {
    e.preventDefault();
    recall();
  } else if (e.key === "Enter" && !e.shiftKey) {
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
