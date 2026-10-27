import { ralTokens } from "./ipc.js";

// Cache exact source strings; highlight bubbles only after settlement to avoid a new key per token.
/** @type {Map<string, import("./bindings/RalToken.ts").RalToken[]>} */
const cache = new Map();
const CACHE_MAX = 64;

// Build text nodes from model output; discard a late response if the element text has changed.
/** @param {Element} el */
export async function highlightRal(el) {
  const src = el.textContent ?? "";
  const tokens = cache.get(src) ?? await fetchTokens(src);
  if (el.textContent !== src) return;
  el.replaceChildren(tokensToNodes(src, tokens));
}

/** @param {string} src */
async function fetchTokens(src) {
  const tokens = await ralTokens(src);
  cache.set(src, tokens);
  for (const oldest of cache.keys()) {
    if (cache.size <= CACHE_MAX) break;
    cache.delete(oldest);
  }
  return tokens;
}

/** @param {string} src @param {import("./bindings/RalToken.ts").RalToken[]} tokens */
function tokensToNodes(src, tokens) {
  const frag = document.createDocumentFragment();
  let at = 0;
  for (const tok of tokens) {
    if (tok.start > at) frag.appendChild(document.createTextNode(src.slice(at, tok.start)));
    const span = document.createElement("span");
    span.className = `tok-${tok.class}`;
    span.appendChild(document.createTextNode(src.slice(tok.start, tok.end)));
    frag.appendChild(span);
    at = tok.end;
  }
  if (at < src.length) frag.appendChild(document.createTextNode(src.slice(at)));
  return frag;
}
