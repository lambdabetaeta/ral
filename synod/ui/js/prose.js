import { renderAssistantMarkdown, mathClosed } from "./math.js";
import { highlightRal } from "./ral-highlight.js";

// Render each settled markdown block once; rebuild only the open tail as tokens arrive.

const settledUpTo = new WeakMap();

export function growProse(bubble, block) {
  const live = liveTail(bubble);
  const from = settledUpTo.get(bubble) ?? 0;
  const tail = block.raw.slice(from);
  const cut = block.open ? settleAt(tail) : tail.length;
  if (cut) {
    live.before(renderChunk(tail.slice(0, cut)));
    settledUpTo.set(bubble, from + cut);
  }
  if (!block.open) {
    live.remove();
    return;
  }
  live.replaceChildren(renderAssistantMarkdown(tail.slice(cut)));
  placeCaret(live);
}

function liveTail(bubble) {
  const found = bubble.querySelector(":scope > .live");
  if (found) return found;
  const live = document.createElement("div");
  live.className = "live";
  bubble.appendChild(live);
  return live;
}

// Highlight only settled code; a changing tail would require a new shell call per token.
function renderChunk(src) {
  const nodes = renderAssistantMarkdown(src);
  for (const code of nodes.querySelectorAll("code.language-ral")) highlightRal(code);
  return nodes;
}

function settleAt(tail) {
  const tokens = marked.lexer(tail);
  if (tokens.length < 2) return 0;
  const settled = tokens.slice(0, openFrom(tokens)).map((t) => t.raw).join("");
  // marked's tokens must partition the source; otherwise no cut is sound.
  if (!tail.startsWith(settled)) return 0;
  return mathClosed(settled) ? settled.length : 0;
}

// Lists and indented code join across blank lines, so a trailing run stays open together.
const CONTINUABLE = new Set(["list", "code"]);

function openFrom(tokens) {
  let open = skipBlank(tokens, tokens.length - 1);
  if (open < 0) return 0;
  while (CONTINUABLE.has(tokens[open].type)) {
    const before = skipBlank(tokens, open - 1);
    if (before < 0 || tokens[before].type !== tokens[open].type) break;
    open = before;
  }
  return open;
}

function skipBlank(tokens, at) {
  while (at >= 0 && tokens[at].type === "space") at -= 1;
  return at;
}

// Place the writing front after the final word; never descend into KaTeX layout.
function placeCaret(live) {
  let host = live;
  while (host.lastElementChild && !host.lastElementChild.classList.contains("katex")) {
    host = host.lastElementChild;
  }
  const caret = document.createElement("span");
  caret.className = "caret";
  caret.setAttribute("aria-hidden", "true");
  host.appendChild(caret);
}
