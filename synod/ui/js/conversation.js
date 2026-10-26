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

/** @typedef {"starting" | "accepted" | "ready" | "ended"} Phase */
/** @typedef {{ attempt: string, phase: Phase }} Attempt */
/** @type {Attempt} */
let assistant = { attempt: "", phase: "ended" };

// Terminal attempts are absorbing; acceptance cannot undo readiness either.
/** @param {Attempt} current @param {string} attempt @param {Exclude<Phase, "starting">} phase */
function advance(current, attempt, phase) {
  if (current.attempt !== attempt || current.phase === "ended") return current;
  if (phase === "accepted" && current.phase !== "starting") return current;
  return { attempt, phase };
}

/** @param {string} attempt @param {Exclude<Phase, "starting">} phase */
function transition(attempt, phase) {
  const next = advance(assistant, attempt, phase);
  if (next === assistant) return false;
  assistant = next;
  refreshComposer();
  return true;
}

function canSend() {
  return !state.busy && (assistant.phase === "accepted" || assistant.phase === "ready");
}

async function startAssistant(folder, choice) {
  const attempt = crypto.randomUUID();
  assistant = { attempt, phase: "starting" };
  state.busy = false;
  refreshComposer();
  try {
    await invoke("start_conversation", { folder: folder.path, choice, attempt });
    // Accepted messages queue until boot; acceptance is not readiness.
    transition(attempt, "accepted");
  } catch (err) {
    if (transition(attempt, "ended")) addSystemMessage(String(err));
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

function setBusy(busy) {
  state.busy = busy;
  refreshComposer();
  setStatus(busy ? "Working…" : "");
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

listen("synod-opening", (event) => {
  const { attempt, opening } = /** @type {import("./bindings/ConversationOpening.ts").ConversationOpening} */ (event.payload);
  if (transition(attempt, "ready")) onOpening(opening);
});
listen("synod-event", (event) => onSynodEvent(event.payload || {}));

listen("exchange-done", async () => {
  closeStreamingProse();
  clearOpenDial();
  setBusy(false);
  await refreshReport();
});

listen("synod-ended", (event) => {
  const { attempt, stopped, explained } = /** @type {import("./bindings/ConversationEnded.ts").ConversationEnded} */ (event.payload);
  if (!transition(attempt, "ended")) return;
  closeStreamingProse();
  clearOpenDial();
  setBusy(false);
  if (!stopped && !explained) addSystemMessage("The assistant stopped unexpectedly. Choose Start again to continue.");
});

function submit() {
  const field = /** @type {HTMLTextAreaElement} */ ($("message"));
  const text = field.value.trim();
  if (!text || !canSend()) return;
  const attempt = assistant.attempt;
  addUserMessage(text);
  field.value = "";
  setBusy(true);
  invoke("send_message", { message: text }).catch((err) => {
    if (assistant.attempt !== attempt || assistant.phase === "ended") return;
    addSystemMessage(String(err));
    setBusy(false);
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
