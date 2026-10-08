import { $, invoke, show } from "./core.js";
import { showError, clearError } from "./start-screen.js";
import { actionButton } from "./ui-helpers.js";

// ---- Accounts screen -------------------------------------------------
//
// A key travels one way only: what comes back from the backend is its
// last four characters and where it is kept, so no stored key is ever
// put into an input here.

export async function loadAccounts() {
  try {
    renderAccounts(await invoke("list_accounts"));
  } catch (err) {
    showError("accounts-error", String(err));
  }
}

// Every command that changes something answers with the whole list as
// it now stands, so the screen redraws from what is true rather than
// from what it hoped. `true` means the change went through.
async function accountCommand(command, args) {
  clearError("accounts-error");
  try {
    renderAccounts(await invoke(command, args));
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

  // The plan's card stands whether or not anyone is signed in; it says
  // which of the two rather than coming and going.
  $("accounts-plan-note").textContent = accounts.some((a) => a.source === "signed_in")
    ? "You are signed in with your ChatGPT plan. Signing in again replaces it."
    : "If you pay for ChatGPT, you can use that plan here instead of a key.";

  // A part-filled add form keeps whatever the user had already chosen.
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

  // Every account that authenticates with a key gets the key input —
  // a declared endpoint included, since a key rotated at the far end
  // has to be retypeable at this one.
  if (account.source !== "signed_in" && account.source !== "keyless") {
    row.appendChild(keyForm(account, i));
  }
  if (account.withdrawable) {
    const actions = document.createElement("div");
    actions.className = "row-actions";
    actions.appendChild(actionButton("Remove", "btn-mini", () =>
      accountCommand("forget_endpoint", { account: account.id })));
    row.appendChild(actions);
  }
  return row;
}

// "Used without a key" is a state the backend states outright, never
// inferred from a missing hint: a service that wants no key must not
// be read as one still waiting for one.
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

// The one input a key is ever typed into: it starts empty and is thrown
// away with the card on the redraw a save brings, so a saved key is
// never left sitting in the window.
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
  // Only a saved key can this window take back.
  if (account.source === "vault" || account.shadowed) {
    actions.appendChild(actionButton("Forget", "btn-mini", () =>
      accountCommand("forget_key", { account: account.id })));
  }

  form.append(label, input, actions);
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    save.disabled = true;
    const saved = await accountCommand("save_key", { account: account.id, key: input.value });
    // A save redraws the whole list, this card with it; a refusal leaves
    // the typed key where it is, to be corrected rather than retyped.
    if (!saved) save.disabled = false;
  });
  return form;
}

$("add-service").addEventListener("submit", async (event) => {
  event.preventDefault();
  const keyless = /** @type {HTMLInputElement} */ ($("add-keyless")).checked;
  /** @type {HTMLButtonElement} */ ($("add-save")).disabled = true;
  // `null` is a server that checks no key; a blank one waits for its variable or a save.
  const added = await accountCommand("save_endpoint", {
    name: /** @type {HTMLInputElement} */ ($("add-name")).value,
    endpoint: /** @type {HTMLInputElement} */ ($("add-address")).value,
    protocol: /** @type {HTMLSelectElement} */ ($("add-protocol")).value,
    key: keyless ? null : /** @type {HTMLInputElement} */ ($("add-key")).value,
  });
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
