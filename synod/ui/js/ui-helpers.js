export function actionButton(label, cls, handler) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = cls;
  b.textContent = label;
  b.addEventListener("click", handler);
  return b;
}
