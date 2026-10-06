import { $, invoke, listen } from "./core.js";
import { enterConversation } from "./conversation.js";

// ---- Start screen ---------------------------------------------------

// The assistant picker: one option per account×model, rebuilt every
// time a menu arrives — the instant `list_models` reply, and the later
// `models-refreshed` event once the live listing is in. Left hidden
// when there is nothing to choose between. `key` is the account's id —
// what a `start_conversation` call names it by — and `label` is only
// ever shown, never sent back: two accounts can share a display label.
let assistantOptions = [];  // [{ key, model, label, reasoning }]
let defaultEffortLabel = "";

// The assistant currently selected, before a render clears and rebuilds
// `assistantOptions` out from under it — read back afterward to decide
// whether the pick survived the rebuild.
function currentAssistantSelection() {
  if (assistantOptions.length === 0) return null;
  const opt = assistantOptions[Number(/** @type {HTMLSelectElement} */ ($("assistant-select")).value)];
  return opt ? { key: opt.key, model: opt.model } : null;
}

// Why a menu offers nothing to run with: no account at all, models still
// on their way, every listing failed (each with its reason), or listings
// that came back empty.
/**
 * @param {import("./bindings/ProviderChoice.ts").ProviderChoice[]} providers
 * @param {boolean} loading
 */
function emptyMenuNote(providers, loading) {
  if (providers.length === 0) {
    return "No assistant account is set up on this computer yet. Sign in with your ChatGPT plan to begin, or add an API key under Accounts.";
  }
  if (loading) {
    return "Loading the models your accounts offer…";
  }
  const failures = providers.flatMap((p) => p.models.state === "failed" ? [p.label + ": " + p.models.reason] : []);
  return failures.length > 0
    ? "Could not list the models your accounts offer (" + failures.join("; ") + "). Check your connection, or sign in again."
    : "Your accounts list no models to work with. Sign in with another account, or add an API key under Accounts.";
}

// The picker's one render path: every menu payload — the instant reply
// and every later refresh alike — is shaped into the assistant and
// effort selects through here, preserving whichever (provider, model)
// pair is already chosen when it still exists in the fresh list rather
// than always resetting to the first.
/** @param {import("./bindings/ModelMenu.ts").ModelMenu | null} menu */
function renderPicker(menu) {
  const providers = (menu && menu.providers) || [];
  const efforts = (menu && menu.efforts) || [];
  defaultEffortLabel = (menu && menu.default_effort) || defaultEffortLabel;

  const previousAssistant = currentAssistantSelection();
  const previousEffort = assistantOptions.length
    ? /** @type {HTMLSelectElement} */ ($("effort-select")).value
    : null;

  assistantOptions = [];
  for (const p of providers) {
    if (p.models.state !== "listed") continue;
    for (const m of p.models.choices) {
      assistantOptions.push({ key: p.account, model: m.name, label: p.label + " — " + m.name, reasoning: m.reasoning });
    }
  }

  const select = /** @type {HTMLSelectElement} */ ($("assistant-select"));
  select.innerHTML = "";
  assistantOptions.forEach((opt, i) => {
    const el = document.createElement("option");
    el.value = String(i);
    el.textContent = opt.label;
    select.appendChild(el);
  });

  const survivedIdx = previousAssistant
    ? assistantOptions.findIndex((o) => o.key === previousAssistant.key && o.model === previousAssistant.model)
    : -1;
  if (assistantOptions.length > 0) select.value = String(Math.max(survivedIdx, 0));

  // Never hide the picker mid-interaction if the current pick
  // survives: hide only when there is nothing left to choose between
  // AND the survivor, if any, is just the first option rather than a
  // deliberate pick.
  const selectedBeyondFirst = survivedIdx > 0;
  $("assistant-field").style.display =
    assistantOptions.length >= 2 || selectedBeyondFirst ? "" : "none";

  const effortSelect = /** @type {HTMLSelectElement} */ ($("effort-select"));
  effortSelect.innerHTML = "";
  efforts.forEach((label) => {
    const el = document.createElement("option");
    el.value = label;
    el.textContent = label;
    effortSelect.appendChild(el);
  });
  effortSelect.value = previousEffort !== null && efforts.includes(previousEffort) ? previousEffort : defaultEffortLabel;

  // The effort control is offered whenever there is any account to
  // offer it for, regardless of how many models that account lists.
  $("effort-field").style.display = providers.length > 0 ? "" : "none";

  // Nothing to answer with is nothing to start: the folder button waits and
  // the note says why. Unless the models are merely loading, the sign-in
  // becomes the way in.
  const nothingToRunWith = assistantOptions.length === 0;
  const loading = providers.some((p) => p.models.state === "loading");
  const needsSignIn = nothingToRunWith && !loading;
  $("menu-note").textContent = nothingToRunWith ? emptyMenuNote(providers, loading) : "";
  $("menu-note").classList.toggle("show", nothingToRunWith);
  $("actions").classList.toggle("needs-sign-in", needsSignIn);
  /** @type {HTMLButtonElement} */ ($("pick-folder")).disabled = nothingToRunWith;
  $("pick-folder").className = needsSignIn ? "btn-quiet" : "btn-primary";
  $("sign-in").className = needsSignIn ? "btn-primary" : "btn-quiet";

  updateEffortDisabled();
}

// The effort control is disabled, not hidden, once the currently
// selected model is positively known not to take one — re-checked here
// on every render and every change to the assistant select.
function updateEffortDisabled() {
  const hidden = $("assistant-field").style.display === "none";
  const idx = hidden ? 0 : Number(/** @type {HTMLSelectElement} */ ($("assistant-select")).value);
  const opt = assistantOptions[idx];
  /** @type {HTMLSelectElement} */ ($("effort-select")).disabled = Boolean(opt) && !opt.reasoning;
}

$("assistant-select").addEventListener("change", updateEffortDisabled);

async function initAssistantPicker() {
  let menu = null;
  try {
    menu = await invoke("list_models");
  } catch (err) {
    // The credential scrub's own failure — a config this computer cannot
    // read. No sign-in mends that, so it is shown rather than swallowed.
    showError("start-error", String(err));
  }
  renderPicker(menu);
  listen("models-refreshed", (event) => renderPicker(event.payload));
}

initAssistantPicker();

// The assistant field can be hidden while still holding the one option
// there was to offer (a single account, a single model): a hidden
// field is not an absent choice, so its option and whatever effort was
// picked for it still have to reach the backend. An empty menu has no
// choice to give, and answers `null`.
function currentChoice() {
  if (assistantOptions.length === 0) return null;
  const hidden = $("assistant-field").style.display === "none";
  const idx = hidden ? 0 : Number(/** @type {HTMLSelectElement} */ ($("assistant-select")).value);
  const opt = assistantOptions[idx];
  if (!opt) return null;
  const effort = $("effort-field").style.display === "none"
    ? defaultEffortLabel
    : /** @type {HTMLSelectElement} */ ($("effort-select")).value;
  return { account: opt.key, model: opt.model, effort };
}

// One error line per screen, shown and cleared by element id.
export function showError(id, text) {
  $(id).textContent = text;
  $(id).classList.add("show");
}

export function clearError(id) {
  $(id).classList.remove("show");
}

$("pick-folder").addEventListener("click", async () => {
  clearError("start-error");
  let chosen;
  try {
    chosen = await invoke("choose_folder");
  } catch (err) {
    showError("start-error", String(err));
    return;
  }
  if (!chosen) return;
  // The menu can empty while the folder dialog is open.
  const choice = currentChoice();
  if (!choice) {
    showError("start-error", "Which assistant should answer? None is on offer yet — sign in, or wait for its models to load.");
    return;
  }
  enterConversation(chosen, choice);
});
