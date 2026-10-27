import { $ } from "./dom.js";

// WebView2 disables native dialogs; use the separately injected plugin and await its result.
export const confirmDialog = window.__TAURI__?.dialog?.confirm ?? (async () => true);

export const state = {
  /** @type {import("./bindings/PickedFolder.ts").PickedFolder | null} */
  folder: null,
  /** @type {import("./bindings/Choice.ts").Choice | null} */
  choice: null,
};

// Pin to the foot while streaming; scrolling up releases it until the reader returns.
export const transcriptEl = $("transcript");
let stickToBottom = true;
transcriptEl.addEventListener("scroll", () => {
  stickToBottom = transcriptEl.scrollHeight - transcriptEl.scrollTop - transcriptEl.clientHeight < 40;
});
export function pinTranscript() {
  if (stickToBottom) transcriptEl.scrollTop = transcriptEl.scrollHeight;
}
