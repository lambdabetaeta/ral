import { $, invoke, listen, screens } from "./core.js";
import { showError, clearError } from "./start-screen.js";
import { loadAccounts } from "./accounts.js";

// ---- Signing in with ChatGPT ----------------------------------------

// While a sign-in is running the one button abandons it rather than
// starting a second: two sign-ins would want the same loopback port, and
// the backend refuses the second anyway.
let signingIn = false;

// The one sign-in stands on two screens at once, so both buttons and
// both lines say the same thing throughout.
const SIGN_IN_BUTTONS = ["sign-in", "accounts-sign-in"];
const SIGN_IN_LINES = ["sign-in-say", "accounts-say"];

function setSigningIn(running) {
  signingIn = running;
  const label = running ? "Stop signing in" : "Sign in with ChatGPT";
  for (const id of SIGN_IN_BUTTONS) $(id).textContent = label;
}

// An error belongs on whichever screen the user is looking at.
function showSignInError(text) {
  if (screens.accounts.classList.contains("active")) showError("accounts-error", text);
  else showError("start-error", text);
}

// The sign-in's own line: a sentence, and — only when the browser could
// not be opened for the user — the link they have to follow themselves,
// both clickable and selectable so either door works.
function saySignIn(text, link) {
  for (const id of SIGN_IN_LINES) fillSignInLine($(id), text, link);
}

function fillSignInLine(el, text, link) {
  el.textContent = text || "";
  if (text && link) {
    const anchor = document.createElement("span");
    anchor.className = "sign-in-link";
    anchor.textContent = link;
    anchor.addEventListener("click", () => {
      invoke("open_url", { url: link }).catch((err) => showSignInError(String(err)));
    });
    el.appendChild(document.createElement("br"));
    el.appendChild(anchor);
  }
  el.classList.toggle("show", Boolean(text));
}

async function startSignIn() {
  if (signingIn) {
    invoke("cancel_sign_in").catch((err) => showSignInError(String(err)));
    return;
  }
  clearError("start-error");
  clearError("accounts-error");
  setSigningIn(true);
  saySignIn("Opening your browser…");
  try {
    await invoke("sign_in");
  } catch (err) {
    setSigningIn(false);
    saySignIn("");
    showSignInError(String(err));
  }
}

for (const id of SIGN_IN_BUTTONS) $(id).addEventListener("click", startSignIn);

listen("sign-in-step", (event) => {
  const step = event.payload || {};
  saySignIn(step.say, step.link);
});

// The menu arrives just before this, as a `models-refreshed` the picker
// renders through its one path — so by the time the account is named
// here, it is already one of the assistants on offer.
listen("sign-in-done", (event) => {
  const done = event.payload || {};
  setSigningIn(false);
  if (done.outcome === "signed_in") {
    saySignIn(
      done.replaced
        ? "Signed in again as " + done.label + "."
        : "Signed in as " + done.label + "."
    );
    if (screens.accounts.classList.contains("active")) loadAccounts();
  } else {
    saySignIn("");
    showSignInError(done.reason || "The sign-in did not finish.");
  }
});
