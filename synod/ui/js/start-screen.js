import { $, showError, clearError } from "./dom.js";
import { listModels, onModelsRefreshed, chooseFolder } from "./ipc.js";
import { enterConversation } from "./conversation.js";

// One option per account/model; keys are ids because display labels may coincide.
let assistantOptions = [];
let defaultEffortLabel = "";

function currentAssistantSelection() {
  if (assistantOptions.length === 0) return null;
  const opt = assistantOptions[Number(/** @type {HTMLSelectElement} */ ($("assistant-select")).value)];
  return opt ? { key: opt.key, model: opt.model } : null;
}

/** @param {import("./bindings/ProviderChoice.ts").ProviderChoice[]} providers @param {boolean} loading */
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

  // Keep the picker visible when a deliberate selection survives a refresh.
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

  $("effort-field").style.display = providers.length > 0 ? "" : "none";

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

// Disable effort only when the selected model is positively known not to support it.
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
    menu = await listModels();
  } catch (err) {
    // No sign-in can repair the startup credential scrub failure.
    showError("start-error", String(err));
  }
  renderPicker(menu);
  onModelsRefreshed(renderPicker);
}

initAssistantPicker();

// A hidden single-option picker still supplies a choice; an empty menu supplies null.
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

$("pick-folder").addEventListener("click", async () => {
  clearError("start-error");
  let chosen;
  try {
    chosen = await chooseFolder();
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
