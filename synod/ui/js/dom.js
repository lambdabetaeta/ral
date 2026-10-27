/** @type {(id: string) => HTMLElement} */
export const $ = (id) => /** @type {HTMLElement} */ (document.getElementById(id));
export const screens = {
  start: $("screen-start"),
  accounts: $("screen-accounts"),
  conversation: $("screen-conversation"),
};

/** @param {keyof typeof screens} name */
export function show(name) {
  for (const [key, el] of Object.entries(screens)) el.classList.toggle("active", key === name);
}

export function actionButton(label, cls, handler) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = cls;
  b.textContent = label;
  b.addEventListener("click", handler);
  return b;
}

export function showError(id, text) {
  const el = $(id);
  el.textContent = text;
  el.classList.add("show");
}

export function clearError(id) {
  $(id).classList.remove("show");
}
