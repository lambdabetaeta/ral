// Chat prose uses GFM and treats a lone newline as a line break.
marked.setOptions({ gfm: true, breaks: true });

// Strip source sentinels before inserting our own; model text must never forge a formula slot.
const SENTINEL_OPEN = "";
const SENTINEL_CLOSE = "";
const SENTINEL = /(\d+)/g;

// Longest delimiter first: $$ must not be read as $.
const MATH_PAIRS = [
  { open: "$$", close: "$$", display: true },
  { open: "\\[", close: "\\]", display: true },
  { open: "\\(", close: "\\)", display: false },
];

// Lift math before markdown, scrub the completed tree last; never serialize and reparse it.
/** @returns {DocumentFragment} */
export function renderAssistantMarkdown(src) {
  const { prose, formulae } = liftMath(src);
  // Template content is inert while the unscrubbed tree is assembled.
  const tree = document.createElement("template");
  tree.innerHTML = marked.parse(prose);
  if (formulae.length) fillMath(tree.content, formulae);
  return DOMPurify.sanitize(tree.content, SCRUB);
}

// Preserve KaTeX accessibility markup; forbid transcript navigation and app-wide styles.
const SCRUB = Object.freeze({
  RETURN_DOM_FRAGMENT: true,
  ADD_TAGS: ["semantics", "annotation"],
  ADD_ATTR: ["columnalign", "columnspacing"],
  FORBID_TAGS: ["form", "area", "map", "base", "meta", "style"],
  FORBID_ATTR: ["formaction", "action", "target"],
});

function liftMath(source) {
  const src = source.split(SENTINEL_OPEN).join("").split(SENTINEL_CLOSE).join("");
  const formulae = [];
  let prose = "";
  let from = 0;
  for (const span of mathSpans(src)) {
    if (span.close === -1 || span.close === span.from) continue;
    prose += src.slice(from, span.start) + SENTINEL_OPEN + formulae.length + SENTINEL_CLOSE;
    formulae.push({
      tex: src.slice(span.from, span.close),
      display: span.display,
      raw: src.slice(span.start, span.end),
    });
    from = span.end;
  }
  return { prose: prose + src.slice(from), formulae };
}

// Rendering and settling share delimiter identity; an absent close stays open.
export function mathClosed(src) {
  for (const span of mathSpans(src)) {
    if (span.close === -1) return false;
  }
  return true;
}

function* mathSpans(src) {
  const code = codeMask(src);
  let at = 0;
  while (at < src.length) {
    if (code[at]) { at++; continue; }
    if (src[at] === "\\" && (src[at + 1] === "$" || src[at + 1] === "\\")) {
      at += 2;
      continue;
    }
    const pair = MATH_PAIRS.find((pair) => src.startsWith(pair.open, at));
    if (pair) {
      const from = at + pair.open.length;
      const close = findOutsideCode(src, code, pair.close, from);
      const end = close === -1 ? src.length : close + pair.close.length;
      yield { start: at, end, from, close, display: pair.display };
      // Even an incomplete longest opener owns its body and its suffix.
      at = end;
      continue;
    }
    const formula = src[at] === "$" ? readDollar(src, code, at) : null;
    if (formula) {
      yield { start: at, ...formula };
      at = formula.end;
    } else at++;
  }
}

// Single-$ math stays tight and on one line; a following digit marks a price range.
function readDollar(src, code, at) {
  const from = at + 1;
  if (from >= src.length || /[\s$]/.test(src[from])) return null;
  for (let end = from; end < src.length; end++) {
    if (src[end] === "\n") return null;
    if (src[end] === "\\") { end++; continue; }
    if (src[end] !== "$" || code[end]) continue;
    if (/\s/.test(src[end - 1])) return null;
    if (/\d/.test(src[end + 1] || "")) return null;
    return { from, close: end, display: false, end: end + 1 };
  }
  return null;
}

function findOutsideCode(src, code, needle, from) {
  for (let at = from; at < src.length; at++) {
    if (code[at]) continue;
    if (src.startsWith(needle, at)) return at;
    if (src[at] === "\\") at++;
  }
  return -1;
}

function codeMask(src) {
  const code = new Uint8Array(src.length);
  let fence = null;
  let at = 0;
  while (at < src.length) {
    const nl = src.indexOf("\n", at);
    const end = nl === -1 ? src.length : nl + 1;
    const run = /^ {0,3}(`{3,}|~{3,})/.exec(src.slice(at, end));
    if (fence) {
      code.fill(1, at, end);
      if (run && run[1][0] === fence[0] && run[1].length >= fence.length) fence = null;
    } else if (run) {
      fence = run[1];
      code.fill(1, at, end);
    } else {
      maskCodeSpans(src, at, end, code);
    }
    at = end;
  }
  return code;
}

function maskCodeSpans(src, from, to, code) {
  let at = from;
  while (at < to) {
    if (src[at] !== "`") { at++; continue; }
    const open = at;
    while (at < to && src[at] === "`") at++;
    const close = backtickRun(src, at, to, at - open);
    if (close === -1) return;
    code.fill(1, open, close);
    at = close;
  }
}

function backtickRun(src, from, to, len) {
  let at = from;
  while (at < to) {
    if (src[at] !== "`") { at++; continue; }
    const open = at;
    while (at < to && src[at] === "`") at++;
    if (at - open === len) return at;
  }
  return -1;
}

// Replace sentinels only in text nodes; attributes must never receive formula nodes.
function fillMath(root, formulae) {
  const walk = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  /** @type {Text[]} */
  const carrying = [];
  while (walk.nextNode()) {
    // SHOW_TEXT guarantees currentNode is a Text node.
    const node = /** @type {Text} */ (walk.currentNode);
    if (node.data.includes(SENTINEL_OPEN)) carrying.push(node);
  }
  for (const node of carrying) {
    // Marked resolves indented code versus list continuation; restore raw math inside code.
    const inCode = node.parentElement !== null && node.parentElement.closest("code, pre") !== null;
    const parts = node.data.split(SENTINEL);
    const filled = document.createDocumentFragment();
    parts.forEach((part, i) => {
      if (i % 2 === 0) {
        if (part) filled.appendChild(document.createTextNode(part));
        return;
      }
      const formula = formulae[Number(part)];
      const typeset = formula && !inCode ? typesetFormula(formula) : null;
      filled.appendChild(typeset ?? document.createTextNode(formula ? formula.raw : ""));
    });
    node.replaceWith(filled);
  }
}

// Cache by formula source; return clones because a node can occupy only one place.
/** @type {Map<string, DocumentFragment | null>} */
const typesetCache = new Map();

function typesetFormula(formula) {
  const key = (formula.display ? "display " : "inline ") + formula.tex;
  if (!typesetCache.has(key)) typesetCache.set(key, katexNodes(formula));
  const nodes = typesetCache.get(key);
  return nodes ? nodes.cloneNode(true) : null;
}

// KaTeX trust stays off, and its generated nodes still pass through the final scrub.
function katexNodes(formula) {
  const holder = document.createElement("span");
  try {
    katex.render(formula.tex, holder, {
      displayMode: formula.display,
      throwOnError: false,
      trust: false,
      strict: false,
    });
  } catch {
    return null;
  }
  const nodes = document.createDocumentFragment();
  nodes.append(...holder.childNodes);
  return nodes;
}
