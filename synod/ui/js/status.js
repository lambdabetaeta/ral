import { $ } from "./dom.js";

/** @param {import("./bindings/Opening.ts").Opening | null} opening */
export function setFacts(opening) {
  const model = opening?.model ?? "";
  const label = opening?.label ?? "";
  $("convo-model").textContent = model;
  $("convo-effort").textContent = opening?.effort ?? "";
  $("convo-model").title = label ? label + " · " + model : "";
  $("convo-facts").classList.toggle("ready", Boolean(opening));
}

export function setStatus(text) {
  const el = $("convo-status");
  el.textContent = "";
  if (!text) return;
  const spinner = document.createElement("span");
  spinner.className = "spinner";
  spinner.setAttribute("aria-hidden", "true");
  el.append(spinner, text);
}
