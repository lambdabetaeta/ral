const TAURI = globalThis.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : () => Promise.reject(new Error("not running in the app"));
const listen = TAURI ? TAURI.event.listen : () => Promise.resolve(() => {});

// The sealed origin live pages run in; window/live.rs serves it.
export const LIVE_FRAME_URL = TAURI ? TAURI.core.convertFileSrc("", "synod-live") : "about:blank";

/** @returns {Promise<import("./bindings/PickedFolder.ts").PickedFolder | null>} */
export function chooseFolder() {
  return invoke("choose_folder");
}

/** @returns {Promise<import("./bindings/ModelMenu.ts").ModelMenu>} */
export function listModels() {
  return invoke("list_models");
}

/** @param {string} folder @param {import("./bindings/Choice.ts").Choice} choice @param {string} attempt @returns {Promise<void>} */
export function startConversation(folder, choice, attempt) {
  return invoke("start_conversation", { folder, choice, attempt });
}

/** @param {string} attempt @param {string} message @returns {Promise<void>} */
export function sendMessage(attempt, message) {
  return invoke("send_message", { attempt, message });
}

/** @param {string} attempt @param {import("./bindings/JobId.ts").JobId} job @returns {Promise<string[]>} */
export function stopJob(attempt, job) {
  return invoke("stop_job", { attempt, job });
}

/** @param {string} attempt @returns {Promise<string[]>} */
export function recallQueued(attempt) {
  return invoke("recall_queued", { attempt });
}

/** @param {number} index @param {string} path @returns {Promise<void>} */
export function openFile(index, path) {
  return invoke("open_file", { index, path });
}

/** @param {string} url @returns {Promise<void>} */
export function openUrl(url) {
  return invoke("open_url", { url });
}

/** @param {string} src @returns {Promise<import("./bindings/RalToken.ts").RalToken[]>} */
export function ralTokens(src) {
  return invoke("highlight_ral", { src });
}

/** @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export function listAccounts() {
  return invoke("list_accounts");
}

/** @param {string} account @param {string} key @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export function saveKey(account, key) {
  return invoke("save_key", { account, key });
}

/** @param {string} account @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export function forgetKey(account) {
  return invoke("forget_key", { account });
}

/** @param {{name: string, endpoint: string, protocol: string, key: string | null}} endpoint @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export function saveEndpoint(endpoint) {
  return invoke("save_endpoint", endpoint);
}

/** @param {string} account @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export function forgetEndpoint(account) {
  return invoke("forget_endpoint", { account });
}

/** @returns {Promise<void>} */
export function signIn() {
  return invoke("sign_in");
}

/** @returns {Promise<void>} */
export function cancelSignIn() {
  return invoke("cancel_sign_in");
}

/** @template T @param {string} name @returns {(call: (payload: T) => void) => Promise<() => void>} */
const on = (name) => (call) => listen(name, (event) => call(event.payload));

/** @type {ReturnType<typeof on<import("./bindings/ConversationEvent.ts").ConversationEvent>>} */
export const onConversationEvent = on("conversation-event");

/** @type {ReturnType<typeof on<import("./bindings/ModelMenu.ts").ModelMenu>>} */
export const onModelsRefreshed = on("models-refreshed");

/** @type {ReturnType<typeof on<import("./bindings/SignInStep.ts").SignInStep>>} */
export const onSignInStep = on("sign-in-step");

/** @type {ReturnType<typeof on<import("./bindings/SignInDone.ts").SignInDone>>} */
export const onSignInDone = on("sign-in-done");
