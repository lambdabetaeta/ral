const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : async () => { throw new Error("not running in the app"); };
const listen = TAURI ? TAURI.event.listen : async () => () => {};

/** @returns {Promise<import("./bindings/PickedFolder.ts").PickedFolder | null>} */
export async function chooseFolder() {
  return invoke("choose_folder");
}

/** @returns {Promise<import("./bindings/ModelMenu.ts").ModelMenu>} */
export async function listModels() {
  return invoke("list_models");
}

/** @param {string} folder @param {import("./bindings/Choice.ts").Choice} choice @param {string} attempt @returns {Promise<void>} */
export async function startConversation(folder, choice, attempt) {
  return invoke("start_conversation", { folder, choice, attempt });
}

/** @param {string} attempt @param {string} message @returns {Promise<void>} */
export async function sendMessage(attempt, message) {
  return invoke("send_message", { attempt, message });
}

/** @param {number} index @param {string} path @returns {Promise<void>} */
export async function openFile(index, path) {
  return invoke("open_file", { index, path });
}

/** @param {string} url @returns {Promise<void>} */
export async function openUrl(url) {
  return invoke("open_url", { url });
}

/** @param {string} src @returns {Promise<import("./bindings/RalToken.ts").RalToken[]>} */
export async function ralTokens(src) {
  return invoke("highlight_ral", { src });
}

/** @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export async function listAccounts() {
  return invoke("list_accounts");
}

/** @param {string} account @param {string} key @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export async function saveKey(account, key) {
  return invoke("save_key", { account, key });
}

/** @param {string} account @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export async function forgetKey(account) {
  return invoke("forget_key", { account });
}

/** @param {{name: string, endpoint: string, protocol: string, key: string | null}} endpoint @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export async function saveEndpoint(endpoint) {
  return invoke("save_endpoint", endpoint);
}

/** @param {string} account @returns {Promise<import("./bindings/AccountList.ts").AccountList>} */
export async function forgetEndpoint(account) {
  return invoke("forget_endpoint", { account });
}

/** @returns {Promise<void>} */
export async function signIn() {
  return invoke("sign_in");
}

/** @returns {Promise<void>} */
export async function cancelSignIn() {
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
