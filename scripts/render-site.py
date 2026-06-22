#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["markdown>=3.5"]
# ///
"""Render the static GitHub Pages site under site/.

One build produces every page; all output is plain static HTML, so the
pages open straight from the filesystem with no server.

  index.html      landing page — download buttons injected from downloads.json
  examples.html   one section per examples/*.ral, highlighted through the ral
                  tree-sitter grammar (regex fallback when the CLI is absent)
  tutorial.html   docs/TUTORIAL.md rendered to HTML
  spec.html       docs/SPEC.md rendered to HTML
  rationale.html  docs/RATIONALE.md rendered to HTML

The examples and doc pages share the doc_page() shell, which links doc.css.
The landing page keeps its own CRT styling in index.template.html.

Run with `uv run scripts/render-site.py` so the markdown dependency above is
resolved automatically.
"""

from __future__ import annotations

import html
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import markdown

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
EXAMPLES_DIR = ROOT / "examples"
DOCS_DIR = ROOT / "docs"

GRAMMAR_DIR = ROOT / "editors" / "tree-sitter-ral"
TS_CONFIG_DIR = GRAMMAR_DIR / ".tsconfig"

CURATED_ORDER = [
    "hello.ral",
    "wc.ral",
    "dedup.ral",
    "rename-extension.ral",
    "find-large.ral",
    "du-by-ext.ral",
    "kv-to-json.ral",
    "csv-summary.ral",
    "atomic-update.ral",
    "hash-tree.ral",
    "fanout-fetch.ral",
    "tail-multi.ral",
    "safe-eval.ral",
]


# ── shared page shell ──────────────────────────────────────────────────────

def doc_page(title: str, body: str, titlebar: str | None = None,
             toc_html: str | None = None) -> str:
    """The reading-page shell: menubar + window + article, styled by doc.css.

    ``titlebar`` is the filename shown in the window's title bar (e.g.
    "spec.md"); ``title`` is the <title> tag and menu link label.
    """
    tb = titlebar or title
    toc = f'\n      <span class="toc">{toc_html}</span>' if toc_html else ""
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>ral — {title}</title>
  <link rel="icon" href="favicon.svg" type="image/svg+xml">
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600;700&display=swap" rel="stylesheet">
  <link rel="stylesheet" href="site.css">
  <link rel="stylesheet" href="doc.css">
  <script>
    (function () {{
      var s = localStorage.getItem('ral-theme');
      if (!s) s = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
      if (s === 'dark') document.documentElement.classList.add('dark');
    }})();
  </script>
</head>
<body>
  <nav class="menubar" aria-label="Documentation">
    <a class="menu-item" href="index.html">ral</a>
    <a class="menu-item" href="tutorial.html">Tutorial</a>
    <a class="menu-item" href="spec.html">Spec</a>
    <a class="menu-item" href="rationale.html">Rationale</a>
    <a class="menu-item" href="examples.html">Examples</a>{toc}
    <span class="spacer"></span>
    <button class="theme-toggle" id="theme-toggle" type="button" aria-label="Switch theme">
      <span class="glyph" id="theme-glyph">&#9788;</span><span id="theme-label">light</span>
    </button>
  </nav>
  <main>
    <div class="window">
      <div class="titlebar">
        <span class="menu-box" aria-hidden="true"></span>
        <span class="title-text">{tb}</span>
      </div>
      <article>
{body}
      </article>
    </div>
  </main>
  <footer class="footer">
    &copy; <a href="https://www.lambdabetaeta.eu">G. A. Kavvos</a>
  </footer>
  <script>
    (function () {{
      var root  = document.documentElement;
      var btn   = document.getElementById('theme-toggle');
      var glyph = document.getElementById('theme-glyph');
      var label = document.getElementById('theme-label');
      function syncToggle() {{
        var dark = root.classList.contains('dark');
        glyph.textContent = dark ? '\u263E' : '\u2600';
        label.textContent = dark ? 'dark' : 'light';
      }}
      syncToggle();
      btn.addEventListener('click', function () {{
        root.classList.toggle('dark');
        localStorage.setItem('ral-theme', root.classList.contains('dark') ? 'dark' : 'light');
        syncToggle();
      }});
    }})();
  </script>
</body>
</html>
"""


# ── landing page ───────────────────────────────────────────────────────────

def render_download_groups(downloads: dict) -> str:
    release_repo = downloads["release_repo"]
    base = f"https://github.com/{release_repo}/releases/download/latest"
    lines: list[str] = []
    for target in downloads["targets"]:
        os_name = html.escape(target["os"])
        primary = html.escape(target["primary"])
        allutils = html.escape(target["allutils"])
        lines.extend(
            [
                "",
                '        <div class="dl-group">',
                f'          <span class="dl-os">{os_name}</span>',
                f'          <a class="dl-btn primary" href="{base}/{primary}" download>{primary}</a>',
                f'          <a class="dl-btn allutils" href="{base}/{allutils}" download>allutils</a>',
                "        </div>",
            ]
        )
    lines.append("")
    return "\n".join(lines)


def render_index() -> None:
    template = (ROOT / "scripts" / "index.template.html").read_text(encoding="utf-8")
    downloads = json.loads((SITE / "downloads.json").read_text(encoding="utf-8"))
    placeholder = "{{DOWNLOAD_GROUPS}}"
    if placeholder not in template:
        raise SystemExit(f"missing placeholder {placeholder} in index.template.html")
    rendered = template.replace(placeholder, render_download_groups(downloads))
    (SITE / "index.html").write_text(rendered, encoding="utf-8")


# ── docs ───────────────────────────────────────────────────────────────────

_BLOCK_CODE_RE = re.compile(r"<pre><code>(.*?)</code></pre>", re.DOTALL)


def highlight_doc_blocks(body: str, use_ts: bool) -> str:
    """Re-highlight every fenced code block in markdown-rendered HTML.

    Markdown emits `<pre><code>…</code></pre>` with the source HTML-escaped;
    the highlighters need raw source and do their own escaping, so the inner
    text is unescaped before highlighting.  Inline `<code>…</code>` spans have
    no preceding `<pre>` and are left untouched.
    """
    def replace(match: re.Match[str]) -> str:
        raw = html.unescape(match.group(1))
        return f'<pre class="source"><code>{highlight_source(raw, use_ts)}</code></pre>'

    return _BLOCK_CODE_RE.sub(replace, body)


def render_docs(use_ts: bool) -> None:
    for src, title, dst in [
        (DOCS_DIR / "TUTORIAL.md", "tutorial", "tutorial.html"),
        (DOCS_DIR / "SPEC.md", "specification", "spec.html"),
        (DOCS_DIR / "RATIONALE.md", "rationale", "rationale.html"),
    ]:
        text = src.read_text(encoding="utf-8")
        # The sources link to each other by repo path; on the site those
        # documents live side by side as pages.
        text = text.replace("SPEC.md", "spec.html")
        text = text.replace("RATIONALE.md", "rationale.html")
        body = markdown.markdown(text, extensions=["extra", "sane_lists"])
        body = highlight_doc_blocks(body, use_ts)
        (SITE / dst).write_text(doc_page(title, body, titlebar=src.name),
                                encoding="utf-8")


# ── examples: splitting ────────────────────────────────────────────────────

def split_example(text: str) -> tuple[str, str]:
    """Return (summary, body) for an example file.

    Summary is the leading `#`-comment block with prefixes stripped (the
    first paragraph the reader sees).  Body is the rest of the file with
    any shebang dropped.  A blank line ends the comment block.
    """
    lines = text.splitlines()
    if lines and lines[0].startswith("#!"):
        lines = lines[1:]
    while lines and not lines[0].strip():
        lines = lines[1:]

    summary_lines: list[str] = []
    i = 0
    while i < len(lines) and lines[i].lstrip().startswith("#"):
        stripped = lines[i].lstrip()
        if stripped.startswith("# "):
            summary_lines.append(stripped[2:])
        elif stripped == "#":
            summary_lines.append("")
        else:
            summary_lines.append(stripped[1:])
        i += 1

    while i < len(lines) and not lines[i].strip():
        i += 1

    summary = "\n".join(summary_lines).strip()
    body = "\n".join(lines[i:]).rstrip() + "\n"
    return summary, body


# ── examples: tree-sitter highlighting ─────────────────────────────────────

def tree_sitter_available() -> bool:
    """Quick precheck: the CLI runs and the parser is generated.

    Probe with `--version` rather than `shutil.which` so an unhealthy
    install fails fast and we fall back to the regex tokeniser cleanly.
    """
    if not (GRAMMAR_DIR / "src" / "parser.c").is_file():
        # The parser is a build artefact (gitignored).  Generate it with the
        # built-in QuickJS runtime so no node/bun is required on the host.
        try:
            subprocess.run(
                ["tree-sitter", "generate", "--js-runtime", "native"],
                cwd=GRAMMAR_DIR,
                capture_output=True,
                check=True,
                timeout=30,
            )
        except (FileNotFoundError, subprocess.CalledProcessError,
                subprocess.TimeoutExpired, OSError) as exc:
            print(f"render-site: tree-sitter generate failed ({exc}); "
                  f"falling back to the regex highlighter.  "
                  f"Install tree-sitter-cli to use the real grammar.",
                  file=sys.stderr)
            return False
        if not (GRAMMAR_DIR / "src" / "parser.c").is_file():
            return False
    try:
        subprocess.run(
            ["tree-sitter", "--version"],
            capture_output=True,
            check=True,
            timeout=5,
        )
    except (FileNotFoundError, subprocess.CalledProcessError,
            subprocess.TimeoutExpired, OSError):
        return False
    return True


_LINE_CELL_RE = re.compile(r"<td class=line>(.*?)</td>", re.DOTALL)


def tree_sitter_highlight(body: str) -> str | None:
    """Highlight `body` through the ral grammar and return class-tagged
    HTML, or None on any failure so the caller can fall back to the regex
    tokeniser.

    The grammar is associated by file extension, so the source is written to
    a temporary `.ral` file.  `highlight --html` emits a document with one
    table row per line; the highlighted code lives in the `<td class=line>`
    cells, which join back into the original source.
    """
    env = {**os.environ, "TREE_SITTER_DIR": str(TS_CONFIG_DIR)}
    with tempfile.NamedTemporaryFile(
        "w", suffix=".ral", delete=False, encoding="utf-8"
    ) as tmp:
        tmp.write(body)
        tmp_path = tmp.name
    try:
        result = subprocess.run(
            ["tree-sitter", "highlight", "--html", "--css-classes", tmp_path],
            cwd=GRAMMAR_DIR,
            env=env,
            capture_output=True,
            text=True,
            check=True,
            timeout=20,
        )
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as exc:
        print(f"tree-sitter: {exc}", file=sys.stderr)
        return None
    finally:
        os.unlink(tmp_path)

    cells = _LINE_CELL_RE.findall(result.stdout)
    return "".join(cells).rstrip("\n") if cells else None


# ── examples: fallback regex tokeniser ─────────────────────────────────────

NUMBER_RE = re.compile(r"\b(\d+(?:\.\d+)?)\b")
VAR_RE    = re.compile(r"(\$[A-Za-z_][\w-]*|\$\[|\$args|\$env|\$script)")
WORD_RE   = re.compile(r"\b([A-Za-z_][\w-]*)\b")

KEYWORDS = {
    "let", "if", "else", "elsif", "for", "while", "case",
    "try", "return", "fail", "exit", "spawn", "await",
    "within", "grant", "use", "source", "true", "false",
    "unit", "not",
}
BUILTINS = {
    "echo", "ls", "cat", "cp", "mv", "rm", "mkdir", "ln",
    "find", "tail", "head", "test", "date", "sha256sum",
    "curl", "git", "docker", "tee", "wc", "printenv",
    "hostname", "whoami", "uname", "pwd", "sleep",
    "from-string", "from-json", "from-lines", "from-lines-list",
    "from-bytes", "to-string", "to-json", "to-lines", "to-bytes",
    "lines", "words", "length", "is-empty", "is-file", "is-dir",
    "is-link", "exists", "equal", "map", "filter", "fold",
    "reduce", "for", "each", "concat", "flat-map", "sort-list",
    "sort-list-by", "reverse", "take", "drop", "first", "zip",
    "enumerate", "keys", "values", "has", "get", "union",
    "intersection", "difference", "sum", "range", "ext", "stem",
    "dir", "base", "path-join", "resolve-path", "file-info",
    "list-dir", "glob", "grep-files", "view", "view-around",
    "line-hash", "edit",
    "temp-dir", "temp-file", "line-count", "intercalate",
    "replace", "re-replace", "re-replace-all", "re-split",
    "re-match", "upper", "lower", "int", "float", "str",
    "par", "race", "watch", "fold-lines", "styled",
    "ansi-red", "ansi-green", "ansi-yellow", "ansi-cyan",
    "ansi-blue", "ansi-magenta", "ansi-bold", "ansi-dim",
    "ansi-reset",
}


def highlight_source(body: str, use_ts: bool) -> str:
    """Highlight ral source, preferring the real grammar and falling back to
    the regex tokeniser when tree-sitter is unavailable or fails."""
    highlighted = tree_sitter_highlight(body) if use_ts else None
    if highlighted is None:
        highlighted = regex_highlight(body)
    return highlighted


def regex_highlight(body: str) -> str:
    """Approximate highlighter used when tree-sitter is unavailable.
    Wraps tokens in `<span class="...">` matching classes defined in
    doc.css.
    """
    out: list[str] = []
    i, n = 0, len(body)
    while i < n:
        ch = body[i]

        if ch == "#":
            end = body.find("\n", i)
            if end == -1:
                end = n
            out.append(f'<span class="comment">{html.escape(body[i:end])}</span>')
            i = end
            continue

        if ch in ("'", '"'):
            quote = ch
            j = i + 1
            while j < n:
                if body[j] == "\\" and j + 1 < n:
                    j += 2
                    continue
                if body[j] == quote:
                    j += 1
                    break
                j += 1
            out.append(f'<span class="string">{html.escape(body[i:j])}</span>')
            i = j
            continue

        m = VAR_RE.match(body, i)
        if m:
            out.append(f'<span class="variable">{html.escape(m.group())}</span>')
            i = m.end()
            continue

        m = NUMBER_RE.match(body, i)
        if m:
            out.append(f'<span class="number">{html.escape(m.group())}</span>')
            i = m.end()
            continue

        m = WORD_RE.match(body, i)
        if m:
            word = m.group()
            if word in KEYWORDS:
                out.append(f'<span class="keyword">{html.escape(word)}</span>')
            elif word in BUILTINS:
                out.append(f'<span class="function-builtin">{html.escape(word)}</span>')
            else:
                out.append(html.escape(word))
            i = m.end()
            continue

        if ch in "|<>!&?=+-*/%":
            out.append(f'<span class="operator">{html.escape(ch)}</span>')
            i += 1
            continue

        out.append(html.escape(ch))
        i += 1
    return "".join(out)


# ── examples: page assembly ────────────────────────────────────────────────

INTRO = (
    "      A guided tour of typical shell tasks written in ral.  Each is a\n"
    "      complete, runnable script — `ral &lt;file&gt;.ral` to execute.  The\n"
    "      comment block at the top explains the bash equivalent and what ral\n"
    "      does differently."
)


def slug(name: str) -> str:
    return re.sub(r"\.ral$", "", name).replace(".", "-")


def render_examples(use_ts: bool) -> None:
    files = sorted(p.name for p in EXAMPLES_DIR.glob("*.ral"))
    ordered: list[str] = []
    seen: set[str] = set()
    for name in CURATED_ORDER:
        if name in files:
            ordered.append(name)
            seen.add(name)
    for name in files:
        if name not in seen:
            ordered.append(name)

    toc_links: list[str] = []
    sections: list[str] = []
    for name in ordered:
        src = EXAMPLES_DIR / name
        text = src.read_text(encoding="utf-8")
        summary, body = split_example(text)

        highlighted = highlight_source(body, use_ts)

        s = slug(name)
        toc_links.append(f'<a href="#{s}">{html.escape(name)}</a>')
        sections.append(
            f'    <section class="example" id="{s}">\n'
            f'      <h2><span class="name">{html.escape(name)}</span></h2>\n'
            f'      <div class="summary">{html.escape(summary)}</div>\n'
            f'      <pre class="source"><code>{highlighted}</code></pre>\n'
            f'    </section>'
        )

    body = (
        "    <h1>examples</h1>\n"
        f'    <p class="intro">\n{INTRO}\n    </p>\n'
        + "\n".join(sections)
    )
    page = doc_page("examples", body, titlebar="examples.ral")
    (SITE / "examples.html").write_text(page, encoding="utf-8")


def main() -> int:
    use_ts = tree_sitter_available()
    if not use_ts:
        print(
            "render-site: tree-sitter not available; falling back to the regex "
            "highlighter.  Install tree-sitter-cli to use the real grammar.",
            file=sys.stderr,
        )
    render_index()
    render_docs(use_ts)
    render_examples(use_ts)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
