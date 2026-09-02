import { $, invoke, listen, state, show } from "./core.js";
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

// Wipe the window's own view of the conversation — the chat document and
// the change report — without touching the running assistant. Shared by
// the first folder pick (which then starts the assistant) and a restart
// (whose assistant is already started by the time this runs: the window
// must never start a second one on top of it).
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
  $("convo-folder").textContent = folder;
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

async function startAssistant(folder, choice) {
  try {
    await invoke("start_conversation", { folder, choice });
    state.alive = true;
  } catch (err) {
    addSystemMessage(String(err));
    state.alive = false;
  }
  // The shell narrates the start itself — "starting", or "finishing the
  // previous session" while it waits for one, through to "ready" at the
  // opening — so this only makes the composer usable again. Not "busy":
  // the input's own disabled state already follows `state.alive`, so a
  // failed start leaves it correctly unusable.
  state.busy = false;
  refreshComposer();
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
  /** @type {HTMLTextAreaElement} */ ($("message")).disabled = state.busy || !state.alive;
  /** @type {HTMLButtonElement} */ ($("send")).disabled = state.busy || !state.alive;
}

// A link the assistant wrote is opened in the user's own browser, never
// followed inside the window — the window's one document is this
// conversation, and navigating it away would throw the conversation out.
// DOMPurify has already dropped any unsafe scheme before the anchor
// reached the DOM.
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

listen("synod-opening", (event) => onOpening(event.payload || {}));
listen("synod-event", (event) => onSynodEvent(event.payload || {}));

listen("exchange-done", async () => {
  closeStreamingProse();
  clearOpenDial();
  setBusy(false);
  await refreshReport();
});

listen("synod-ended", (event) => {
  const { stopped, explained } = event.payload || {};
  state.alive = false;
  closeStreamingProse();
  clearOpenDial();
  setBusy(false); // input disables on `state.alive` alone; nothing is "working"
  if (!stopped && !explained) addSystemMessage("The assistant stopped unexpectedly. Choose Start again to continue.");
});

function submit() {
  const field = /** @type {HTMLTextAreaElement} */ ($("message"));
  const text = field.value.trim();
  if (!text || state.busy || !state.alive) return;
  addUserMessage(text);
  field.value = "";
  setBusy(true);
  invoke("send_message", { message: text }).catch((err) => {
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
  if (!confirm("Start again? This begins a fresh conversation; nothing already " +
    "changed in your folder is undone by this.")) return;
  /** @type {HTMLButtonElement} */ ($("start-again")).disabled = true;
  // Restarting is the same act as starting: `start_conversation`
  // supersedes whatever is running, over the selection this conversation
  // was begun with.
  resetTranscript(state.folder);
  await startAssistant(state.folder, state.choice);
  /** @type {HTMLButtonElement} */ ($("start-again")).disabled = false;
});
