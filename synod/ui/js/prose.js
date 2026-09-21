import { renderAssistantMarkdown, codeMask } from "./math.js";
import { highlightRal } from "./ral-highlight.js";

// ---- Streaming prose --------------------------------------------------
//
// An assistant bubble is a document with a settled prefix and one block
// still being written. Re-parsing the whole message per token is quadratic
// in it (measured: 3.3s of main thread by 8,000 tokens), and rebuilding the
// bubble reflows prose the reader is already reading and discards KaTeX
// already laid out. So the source is cut at markdown block boundaries:
// what lies before the open block is rendered once and never touched
// again, and only the tail is rebuilt as tokens land.

// How much of a bubble's source has become settled DOM above its live
// tail. Held per element, because the projector owns the blocks.
const settledUpTo = new WeakMap();

// Bring `bubble` up to date with its block — cheap enough for every token,
// since the work is one block's worth of markdown.
export function growProse(bubble, block) {
  const live = liveTail(bubble);
  const from = settledUpTo.get(bubble) ?? 0;
  const tail = block.raw.slice(from);
  const cut = block.open ? settleAt(tail) : tail.length;
  if (cut) {
    live.before(renderChunk(tail.slice(0, cut)));
    settledUpTo.set(bubble, from + cut);
  }
  // A closed bubble has no tail at all: every block in it has settled, and
  // the bubble's closing margin reads off the last of them.
  if (!block.open) {
    live.remove();
    return;
  }
  live.replaceChildren(renderChunk(tail.slice(cut)));
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

// A run of closed blocks as nodes — the one rendering each settled block
// ever gets, so its code is coloured and its formulae typeset just once.
function renderChunk(src) {
  const holder = document.createElement("template");
  holder.innerHTML = renderAssistantMarkdown(src);
  for (const code of holder.content.querySelectorAll("code.language-ral")) highlightRal(code);
  return holder.content;
}

// How much of `tail` is closed markdown: everything before the run of
// blocks still open at its end.
function settleAt(tail) {
  const tokens = marked.lexer(tail);
  if (tokens.length < 2) return 0;
  const settled = tokens.slice(0, openFrom(tokens)).map((t) => t.raw).join("");
  // marked's tokens partition their source. Anything else is not a cut
  // this understands, so nothing settles.
  if (!tail.startsWith(settled)) return 0;
  return mathClosed(settled) ? settled.length : 0;
}

// The two block kinds a later block of the same kind joins rather than
// follows: list items and indented code both reach across a blank line.
// A trailing run of them therefore stays open together; every other block
// closes the moment another follows it.
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

// The last token at or before `at` that is not blank lines.
function skipBlank(tokens, at) {
  while (at >= 0 && tokens[at].type === "space") at -= 1;
  return at;
}

// `math.js` lifts a formula out of the source before marked ever sees it,
// so no cut may fall between a formula's delimiters: a chunk settles only
// once the multi-line mathematics opened inside it has closed there too.
function mathClosed(src) {
  const code = codeMask(src);
  let dollars = 0;
  let brackets = 0;
  for (let at = 0; at < src.length; at += 1) {
    if (code[at]) continue;
    if (src.startsWith("$$", at)) {
      dollars += 1;
      at += 1;
    } else if (src.startsWith("\\[", at)) brackets += 1;
    else if (src.startsWith("\\]", at)) brackets -= 1;
  }
  return dollars % 2 === 0 && brackets === 0;
}

// The writing front: a mark at the end of the tail, not an animation over
// the prose. It goes inside the deepest last element so that it follows
// the final word rather than standing on a line of its own; KaTeX's markup
// is the one thing it will not descend into, having its own metrics.
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
