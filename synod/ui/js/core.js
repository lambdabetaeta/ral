// The no-bundler bridge: withGlobalTauri exposes these on the window.
const TAURI = window.__TAURI__;
export const invoke = TAURI ? TAURI.core.invoke : async () => { throw new Error("not running in the app"); };
export const listen = TAURI ? TAURI.event.listen : async () => () => {};
// The plugin's confirm, not the window's: WebView2 disables native script
// dialogs, so `window.confirm` there answers `undefined` — and a caller
// reading that as "the user declined" makes its own button do nothing at
// all, silently.  This one returns a real `Promise<boolean>`, so `await` it.
// Optional-chained rather than gated on `TAURI` alone: the plugin's global
// is injected separately from the core one, and reaching through a missing
// `dialog` at module load would take the whole window down over a
// confirmation.  Answering `true` is the safe absence — this dialog guards
// an act that changes nothing by itself, so a missing plugin should leave the button
// working, not dead.
export const confirmDialog = TAURI?.dialog?.confirm ?? (async () => true);

// Every id passed here is written in index.html's own markup, so the element
// is present or the window is broken beyond a null check's help.  The cast
// states that, once, instead of every caller answering `possibly null`.
/** @type {(id: string) => HTMLElement} */
export const $ = (id) => /** @type {HTMLElement} */ (document.getElementById(id));
export const screens = {
  start: $("screen-start"),
  accounts: $("screen-accounts"),
  conversation: $("screen-conversation"),
};

export function show(name) {
  for (const [key, el] of Object.entries(screens)) el.classList.toggle("active", key === name);
}

export const state = {
  /** @type {string | null} */
  folder: null,
  choice: null,  // { provider, model, effort } picked on the start screen, or null for the default
  busy: false,   // an exchange is in flight
  alive: false,  // the assistant is currently running
};

// The transcript is its own scroll region — a chat-only scrollbar,
// separate from the change report's. It sticks to the foot as tokens
// and cards stream in, and lets go the moment the reader scrolls up to
// look back, re-engaging once they return to the bottom.
export const transcriptEl = $("transcript");
let stickToBottom = true;
transcriptEl.addEventListener("scroll", () => {
  stickToBottom = transcriptEl.scrollHeight - transcriptEl.scrollTop - transcriptEl.clientHeight < 40;
});
export function pinTranscript() {
  if (stickToBottom) transcriptEl.scrollTop = transcriptEl.scrollHeight;
}
