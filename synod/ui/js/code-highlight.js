import { highlightRal } from "./ral-highlight.js";

// Highlight fenced code by its declared language; never guess one.
// Runs on scrubbed nodes: hljs escapes its input, and the source is read back as text.
/** @param {ParentNode} root */
export function highlightCode(root) {
  for (const code of root.querySelectorAll("pre > code")) {
    const lang = languageOf(code);
    if (lang === "ral") highlightRal(code);
    else if (lang && hljs.getLanguage(lang)) {
      code.innerHTML = hljs.highlight(code.textContent ?? "", { language: lang, ignoreIllegals: true }).value;
    }
  }
}

/** @param {Element} code */
function languageOf(code) {
  for (const cls of code.classList) if (cls.startsWith("language-")) return cls.slice("language-".length);
  return undefined;
}
