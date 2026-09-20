import { invoke } from "./core.js";

// Keyed on the exact source string: the dial streams and the assistant
// bubble both re-render on their own cadence, and a source already seen
// costs no round trip to the shell the second time. A growing block is a
// fresh key on every flush, so the oldest go once the window is reached.
/** @type {Map<string, import("./bindings/RalToken.ts").RalToken[]>} */
const cache = new Map();
const CACHE_MAX = 64;

// Colour `el`'s ral source in place. `el`'s whole `textContent` is taken as
// one script; its children are rebuilt as text nodes and
// `<span class="tok-...">`s, never through `innerHTML` — the source is
// model output. `el` may already have moved on to different text by the
// time the shell answers (the caller can be mid-stream), so the result is
// dropped rather than applied when that happens.
/** @param {Element} el */
export async function highlightRal(el) {
  const src = el.textContent ?? "";
  const tokens = cache.get(src) ?? await fetchTokens(src);
  if (el.textContent !== src) return;
  el.replaceChildren(tokensToNodes(src, tokens));
}

/** @param {string} src */
async function fetchTokens(src) {
  const tokens = await invoke("highlight_ral", { src });
  cache.set(src, tokens);
  for (const oldest of cache.keys()) {
    if (cache.size <= CACHE_MAX) break;
    cache.delete(oldest);
  }
  return tokens;
}

/**
 * @param {string} src
 * @param {import("./bindings/RalToken.ts").RalToken[]} tokens
 */
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
