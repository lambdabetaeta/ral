import { $, screens, showError, clearError } from "./dom.js";
import { openUrl, signIn, cancelSignIn, onSignInStep, onSignInDone } from "./ipc.js";
import { loadAccounts } from "./accounts.js";

// A running sign-in makes the button cancel; two attempts would compete for the loopback port.
let signingIn = false;

const SIGN_IN_BUTTONS = ["sign-in", "accounts-sign-in"];
const SIGN_IN_LINES = ["sign-in-say", "accounts-say"];

function setSigningIn(running) {
  signingIn = running;
  const label = running ? "Stop signing in" : "Sign in with ChatGPT";
  for (const id of SIGN_IN_BUTTONS) $(id).textContent = label;
}

function showSignInError(text) {
  if (screens.accounts.classList.contains("active")) showError("accounts-error", text);
  else showError("start-error", text);
}

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
      openUrl(link).catch((err) => showSignInError(String(err)));
    });
    el.appendChild(document.createElement("br"));
    el.appendChild(anchor);
  }
  el.classList.toggle("show", Boolean(text));
}

async function startSignIn() {
  if (signingIn) {
    cancelSignIn().catch((err) => showSignInError(String(err)));
    return;
  }
  clearError("start-error");
  clearError("accounts-error");
  setSigningIn(true);
  saySignIn("Opening your browser…");
  try {
    await signIn();
  } catch (err) {
    setSigningIn(false);
    saySignIn("");
    showSignInError(String(err));
  }
}

for (const id of SIGN_IN_BUTTONS) $(id).addEventListener("click", startSignIn);

onSignInStep((step) => {
  saySignIn(step.say, step.link);
});

// The menu event precedes completion, so the named account is already on offer.
onSignInDone((done) => {
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
