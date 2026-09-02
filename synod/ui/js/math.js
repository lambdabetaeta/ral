// Chat prose, not a document: a lone newline is a line break, and GFM
// (tables, fenced code, autolinks) is what the assistant writes in.
marked.setOptions({ gfm: true, breaks: true });

// ---- Mathematics ----------------------------------------------------
//
// A formula must be lifted out of the source *before* marked sees it,
// for two reasons that each corrupt it silently: markdown reads
// `x_1 * x_2` as emphasis and eats the marks, and `breaks: true` cuts a
// multi-line `$$` block with a `<br>` that no delimiter scan can then
// see across. So each formula leaves a sentinel behind, the prose around
// it is parsed and scrubbed exactly as before, and the typeset formula
// is put back into the finished tree.
//
// Sentinels are private-use code points, and any the model itself sends
// are dropped from the source first — so text can never forge one.
const SENTINEL_OPEN = "";
const SENTINEL_CLOSE = "";
const SENTINEL = /(\d+)/g;

// The paired delimiters, tried longest first so `$$` is never read as a
// `$`. The lone `$` is a case apart — see readDollar.
const MATH_PAIRS = [
  { open: "$$", close: "$$", display: true },
  { open: "\\[", close: "\\]", display: true },
  { open: "\\(", close: "\\)", display: false },
];

// The one path model text takes to the DOM: lift the mathematics out,
// parse the prose, scrub it, then typeset the formulae into the result.
// Never assign assistant output to innerHTML without passing through
// here.
export function renderAssistantMarkdown(src) {
  const { prose, formulae } = liftMath(src);
  const html = DOMPurify.sanitize(marked.parse(prose));
  if (!formulae.length) return html;
  const tree = document.createElement("template");
  tree.innerHTML = html;
  fillMath(tree.content, formulae);
  return tree.innerHTML;
}

// The source with every formula lifted out: the prose left behind, and
// the formulae in the order their sentinels number them.
function liftMath(source) {
  const src = source.split(SENTINEL_OPEN).join("").split(SENTINEL_CLOSE).join("");
  const code = codeMask(src);
  const formulae = [];
  let prose = "";
  let at = 0;
  while (at < src.length) {
    // An escape stands for its own character: `\$` is a dollar sign, and
    // marked is the one that unescapes it. Stepping over both characters
    // also stops `\\[` being misread as an opening `\[`.
    if (!code[at] && src[at] === "\\" && (src[at + 1] === "$" || src[at + 1] === "\\")) {
      prose += src.substr(at, 2);
      at += 2;
      continue;
    }
    const formula = code[at] ? null : readMath(src, code, at);
    if (!formula) {
      prose += src[at++];
      continue;
    }
    prose += SENTINEL_OPEN + formulae.length + SENTINEL_CLOSE;
    formulae.push(formula);
    at = formula.end;
  }
  return { prose, formulae };
}

// The formula opening at `at`, or null. An unterminated one is not a
// formula — which is also what keeps a half-arrived `$$` from flashing
// an error at the reader while it streams.
function readMath(src, code, at) {
  for (const pair of MATH_PAIRS) {
    if (!src.startsWith(pair.open, at)) continue;
    const from = at + pair.open.length;
    const close = findOutsideCode(src, code, pair.close, from);
    if (close === -1 || close === from) continue;
    return {
      tex: src.slice(from, close),
      display: pair.display,
      raw: src.slice(at, close + pair.close.length),
      end: close + pair.close.length,
    };
  }
  return src[at] === "$" ? readDollar(src, code, at) : null;
}

// Single-`$` mathematics, the delimiter that also spells money and shell
// variables. Three rules keep prose out: the formula opens and closes
// tight (no space beside either `$`), it stays on its own line, and a
// digit straight after the closing `$` means the pair was a price range
// (`$5-$10`), not a formula. Together these leave `$file has $nlines`
// and `it costs $100 or $200` alone.
function readDollar(src, code, at) {
  const from = at + 1;
  if (from >= src.length || /[\s$]/.test(src[from])) return null;
  for (let end = from; end < src.length; end++) {
    if (src[end] === "\n") return null;
    if (src[end] === "\\") { end++; continue; }
    if (src[end] !== "$" || code[end]) continue;
    if (/\s/.test(src[end - 1])) return null;
    if (/\d/.test(src[end + 1] || "")) return null;
    return { tex: src.slice(from, end), display: false, raw: src.slice(at, end + 1), end: end + 1 };
  }
  return null;
}

function findOutsideCode(src, code, needle, from) {
  for (let at = from; (at = src.indexOf(needle, at)) !== -1; at++) {
    if (!code[at]) return at;
  }
  return -1;
}

// Where a `$` is only a dollar sign: fenced code blocks and inline code
// spans, which is where this app's own shell examples keep theirs.
function codeMask(src) {
  const code = new Uint8Array(src.length);
  let fence = null;                    // the ``` or ~~~ run that opened the block
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

// Inline code within one line: a run of backticks closed by a run of the
// same length. A run left open where the line ends opened nothing.
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

// The end of the next run of exactly `len` backticks in [from, to).
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

// Sentinels sit in text nodes — one node may hold several, with prose
// between them — and each becomes its typeset formula in place. Only
// text nodes are visited, so a sentinel that ended up in an attribute
// (mathematics inside a link's target, say) is left as the invisible
// character it is rather than breaking the markup around it.
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
    // The mask before marked knows only fences and backticks, because
    // whether four spaces open a code block or continue a list item is a
    // question only a block parser can answer. Marked has now answered
    // it, and inside its verdict a formula is not one, however the
    // delimiters looked: it goes back as the text it was written as.
    const inCode = node.parentElement !== null && node.parentElement.closest("code, pre") !== null;
    const parts = node.data.split(SENTINEL);   // prose, index, prose, index, …
    const filled = document.createDocumentFragment();
    parts.forEach((part, i) => {
      if (i % 2 === 0) {
        if (part) filled.appendChild(document.createTextNode(part));
        return;
      }
      const formula = formulae[Number(part)];
      const html = formula && !inCode ? typeset(formula) : null;
      if (html === null) {
        filled.appendChild(document.createTextNode(formula ? formula.raw : ""));
        return;
      }
      const holder = document.createElement("template");
      holder.innerHTML = html;                 // KaTeX's own spans, unwrapped
      filled.appendChild(holder.content);
    });
    node.replaceWith(filled);
  }
}

// Streaming rebuilds the whole bubble on every token, so a formula the
// reader is already looking at is typeset once and then remembered.
const typesetCache = new Map();

function typeset(formula) {
  const key = (formula.display ? "display " : "inline ") + formula.tex;
  if (!typesetCache.has(key)) typesetCache.set(key, katexHtml(formula));
  return typesetCache.get(key);
}

// KaTeX with `trust` off can emit neither a link nor a raw node, so its
// output carries no markup from the model. That is why the formula is put
// back *after* the scrub instead of through it: DOMPurify's CSS filter
// would strip the inline metrics the layout is entirely made of.
function katexHtml(formula) {
  try {
    return katex.renderToString(formula.tex, {
      displayMode: formula.display,
      throwOnError: false,   // a mistyped formula shows in red, in place
      trust: false,
      strict: false,         // chat TeX is not a journal's TeX
    });
  } catch (err) {
    return null;             // beyond even KaTeX's own error rendering
  }
}
