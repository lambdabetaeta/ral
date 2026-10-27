import { $, show, showError, clearError, actionButton } from "./dom.js";
import { listAccounts, saveKey, forgetKey, saveEndpoint, forgetEndpoint } from "./ipc.js";

// Keys travel one way: only their last four characters return from the backend.

export async function loadAccounts() {
  try {
    renderAccounts(await listAccounts());
  } catch (err) {
    showError("accounts-error", String(err));
  }
}

/** @param {() => Promise<import("./bindings/AccountList.ts").AccountList>} call */
async function accountCommand(call) {
  clearError("accounts-error");
  try {
    renderAccounts(await call());
    return true;
  } catch (err) {
    showError("accounts-error", String(err));
    return false;
  }
}

function renderAccounts(list) {
  const accounts = (list && list.accounts) || [];
  const vault = (list && list.vault) || "";
  $("accounts-vault").textContent = vault
    ? "Keys are kept in " + vault + ". Once saved, a key is never shown here again."
    : "";

  const holder = $("accounts-list");
  holder.innerHTML = "";
  if (accounts.length === 0) {
    const empty = document.createElement("p");
    empty.className = "accounts-empty";
    empty.textContent = "No services are set up on this computer yet.";
    holder.appendChild(empty);
  }
  accounts.forEach((account, i) => holder.appendChild(accountRow(account, i)));

  $("accounts-plan-note").textContent = accounts.some((a) => a.source === "signed_in")
    ? "You are signed in with your ChatGPT plan. Signing in again replaces it."
    : "If you pay for ChatGPT, you can use that plan here instead of a key.";

  const select = /** @type {HTMLSelectElement} */ ($("add-protocol"));
  const chosen = select.value;
  const protocols = (list && list.protocols) || [];
  select.innerHTML = "";
  for (const protocol of protocols) {
    const option = document.createElement("option");
    option.value = protocol;
    option.textContent = protocol;
    select.appendChild(option);
  }
  if (protocols.includes(chosen)) select.value = chosen;
}

function accountRow(account, i) {
  const row = document.createElement("div");
  row.className = "account";

  const head = document.createElement("div");
  head.className = "account-head";
  const name = document.createElement("h2");
  name.className = "account-name";
  name.textContent = account.label;
  const state = document.createElement("span");
  state.className = "account-state";
  state.textContent = keyState(account);
  head.append(name, state);
  row.appendChild(head);

  for (const line of accountNotes(account)) {
    const note = document.createElement("p");
    note.className = "account-note";
    note.textContent = line;
    row.appendChild(note);
  }

  if (account.source !== "signed_in" && account.source !== "keyless") {
    row.appendChild(keyForm(account, i));
  }
  if (account.withdrawable) {
    const actions = document.createElement("div");
    actions.className = "row-actions";
    actions.appendChild(actionButton("Remove", "btn-mini", () =>
      accountCommand(() => forgetEndpoint(account.id))));
    row.appendChild(actions);
  }
  return row;
}

// Keyless is explicit; a missing hint does not imply a service needs no key.
function keyState(account) {
  if (account.source === "signed_in") return "Signed in";
  if (account.source === "keyless") return "Used without a key";
  return account.hint ? "•••• " + account.hint : "No key yet";
}

function accountNotes(account) {
  const notes = [];
  if (account.endpoint) notes.push("Address: " + account.endpoint);
  if (account.protocol) notes.push("Way of speaking: " + account.protocol);
  switch (account.source) {
    case "signed_in":
      notes.push("You are signed in with your ChatGPT plan, so this one needs no key.");
      break;
    case "keyless":
      notes.push("This one asks for no key, so there is none to type.");
      break;
    case "environment":
      notes.push(
        "This key comes from " + account.env_var + " in the environment synod was"
          + " started in, and is used ahead of any key saved here."
          + (account.shadowed
            ? " The saved key (•••• " + account.shadowed + ") is used when it isn't set."
            : ""),
      );
      break;
    case "none":
      notes.push("Paste the key this service gave you to start using it.");
      break;
  }
  return notes;
}

// Key inputs start empty and are discarded on save, leaving no saved secret in the window.
function keyForm(account, i) {
  const form = document.createElement("form");
  form.className = "account-form";
  const inputId = "account-key-" + i;

  const label = document.createElement("label");
  label.className = "field-label";
  label.htmlFor = inputId;
  label.textContent = account.source === "vault"
    ? "Replace the saved key for " + account.label
    : account.source === "environment"
    ? "Save a key for " + account.label + ", for when " + account.env_var + " isn't set"
    : "Key for " + account.label;

  const input = document.createElement("input");
  input.type = "password";
  input.id = inputId;
  input.className = "text-input";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.placeholder = "Paste the key here";

  const actions = document.createElement("div");
  actions.className = "row-actions";
  const save = document.createElement("button");
  save.type = "submit";
  save.className = "btn-mini keep";
  save.textContent = "Save";
  actions.appendChild(save);
  if (account.source === "vault" || account.shadowed) {
    actions.appendChild(actionButton("Forget", "btn-mini", () =>
      accountCommand(() => forgetKey(account.id))));
  }

  form.append(label, input, actions);
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    save.disabled = true;
    const saved = await accountCommand(() => saveKey(account.id, input.value));
    // Save redraws the list; a refusal keeps the typed key available for correction.
    if (!saved) save.disabled = false;
  });
  return form;
}

$("add-service").addEventListener("submit", async (event) => {
  event.preventDefault();
  const keyless = /** @type {HTMLInputElement} */ ($("add-keyless")).checked;
  /** @type {HTMLButtonElement} */ ($("add-save")).disabled = true;
  // null is a keyless server; a blank key waits for the environment or a save.
  const added = await accountCommand(() => saveEndpoint({
    name: /** @type {HTMLInputElement} */ ($("add-name")).value,
    endpoint: /** @type {HTMLInputElement} */ ($("add-address")).value,
    protocol: /** @type {HTMLSelectElement} */ ($("add-protocol")).value,
    key: keyless ? null : /** @type {HTMLInputElement} */ ($("add-key")).value,
  }));
  /** @type {HTMLButtonElement} */ ($("add-save")).disabled = false;
  if (added) {
    for (const id of ["add-name", "add-address", "add-key"]) {
      /** @type {HTMLInputElement} */ ($(id)).value = "";
    }
    /** @type {HTMLInputElement} */ ($("add-keyless")).checked = false;
    /** @type {HTMLInputElement} */ ($("add-key")).disabled = false;
  }
});

$("add-keyless").addEventListener("change", (event) => {
  /** @type {HTMLInputElement} */ ($("add-key")).disabled =
    /** @type {HTMLInputElement} */ (event.target).checked;
});

$("open-accounts").addEventListener("click", () => {
  clearError("accounts-error");
  show("accounts");
  loadAccounts();
});

$("accounts-done").addEventListener("click", () => show("start"));
