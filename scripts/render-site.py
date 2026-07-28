#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["markdown>=3.5"]
# ///
"""Render the static GitHub Pages site under site/.

One build produces every page; all output is plain static HTML, so the
pages open straight from the filesystem with no server.

  index.html      landing page — downloads and the lead comparison injected
                  into index.template.html
  examples.html   an explorer over examples/: a picker on the left, and on the
                  right the chosen task written twice, in bash and in ral
  tutorial.html   docs/TUTORIAL.md rendered to HTML
  spec.html       docs/SPEC.md rendered to HTML
  rationale.html  docs/RATIONALE.md rendered to HTML

  exarch/index.html     the exarch landing — shared menubar injected into
                        exarch-index.template.html
  exarch/profiles.html  exarch/PROFILES.md rendered to HTML (single source of
                        truth, so the page can't drift from the docs)

Every ral listing is highlighted through the project's own tree-sitter
grammar, which decides what a keyword or a builtin is by parsing.  The CLI is
therefore a hard requirement: a build that cannot reach it fails rather than
publishing source coloured by a guess.

The examples and doc pages share the doc_page() shell, which links doc.css.
The exarch sub-site has its own chrome under exarch/ and is not styled here.

Run with `uv run scripts/render-site.py` so the markdown dependency above is
resolved automatically.
"""

from __future__ import annotations

import hashlib
import html
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import markdown

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
EXAMPLES_DIR = ROOT / "examples"
DOCS_DIR = ROOT / "docs"
EXARCH_DIR = ROOT / "exarch"
EXARCH_SITE = SITE / "exarch"

GRAMMAR_DIR = ROOT / "editors" / "tree-sitter-ral"
TS_CONFIG_DIR = GRAMMAR_DIR / ".tsconfig"

# The tour opens on these; everything else follows alphabetically.
CURATED_ORDER = [
    "hello", "wc", "dedup", "rename-extension", "find-large", "du-by-ext",
    "kv-to-json", "csv-summary", "atomic-update", "hash-tree", "fanout-fetch",
    "tail-multi", "safe-eval",
]

# The landing argues on both sides of the value/command boundary.
# Keep these as examples, not copied snippets, so the homepage always shows
# the same programs as the examples explorer.
LANDING_EXAMPLES = {
    "effect": {
        "slug": "mock-command",
        "sh_start": "# Mock:",
        "ral_start": "within [",
    },
    "value": {
        "slug": "hello",
        "sh_start": 'for name in "$@"; do',
        "ral_start": "let names =",
    },
}

SANDBOX_EXAMPLE = """grant [
    exec: [git: ['status']],
    fs: [read: ['cwd:'], write: []],
    net: false,
] {
    git status
}"""

NAV = [
    ("index.html", "ral", "index"),
    ("tutorial.html", "Tutorial", "tutorial"),
    ("spec.html", "Spec", "spec"),
    ("rationale.html", "Rationale", "rationale"),
    ("examples.html", "Examples", "examples"),
    ("wiki/index.html", "Wiki", "wiki"),
]

THEME_BOOT = """  <script>
    (function () {
      var s = localStorage.getItem('ral-theme');
      if (!s) s = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
      if (s === 'dark') document.documentElement.classList.add('dark');
    })();
  </script>"""

CHROME_SCRIPT = """  <script>
    (function () {
      var root  = document.documentElement;
      var btn   = document.getElementById('theme-toggle');
      var glyph = document.getElementById('theme-glyph');
      var label = document.getElementById('theme-label');
      function sync() {
        var dark = root.classList.contains('dark');
        glyph.textContent = dark ? '\\u263e' : '\\u2600';
        label.textContent = dark ? 'Night' : 'Workspace';
      }
      sync();
      btn.addEventListener('click', function () {
        root.classList.toggle('dark');
        localStorage.setItem('ral-theme', root.classList.contains('dark') ? 'dark' : 'light');
        sync();
      });

      var time = document.getElementById('fp-time');
      var date = document.getElementById('fp-date');
      function tick() {
        var d = new Date();
        time.textContent = ('0' + d.getHours()).slice(-2) + ':' + ('0' + d.getMinutes()).slice(-2);
        date.textContent = d.toLocaleDateString(undefined,
          { weekday: 'short', day: 'numeric', month: 'short' });
      }
      tick();
      setInterval(tick, 20000);
    })();
  </script>"""

# The front panel's workspaces are the pages, each with its own X11 hue.
PANEL_TILES = [
    ("index.html", "ral", "index", "teal", "&#9636;"),
    ("tutorial.html", "Tutorial", "tutorial", "gold", "&#9637;"),
    ("spec.html", "Spec", "spec", "sea", "&#9638;"),
    ("rationale.html", "Rationale", "rationale", "coral", "&#9639;"),
    ("examples.html", "Examples", "examples", "orchid", "&#9640;"),
]

LICENSE_STATUS = (
    '<span class="fp-license">'
    '<a href="https://spdx.org/licenses/MIT">'
    '<span class="fp-license-icon" style="--c: var(--teal)" aria-hidden="true">'
    '&#9638;</span>MIT</a>'
    '<a href="https://www.apache.org/licenses/LICENSE-2.0">'
    '<span class="fp-license-icon" style="--c: var(--gold)" aria-hidden="true">'
    '&#9638;</span>Apache-2.0</a></span>'
)


def front_panel(current: str, status: str) -> str:
    """The desktop's bar: a workspace switcher over the pages, a clock, and
    the status readout.  The clock is filled in by CHROME_SCRIPT."""
    tiles = "\n".join(
        f'          <a class="fp-tile{" current" if key == current else ""}" href="{href}">'
        f'<span class="ico" style="--c: var(--{hue})" aria-hidden="true">{glyph}</span>{label}</a>'
        for href, label, key, hue, glyph in PANEL_TILES
    )
    return f"""    <div class="fp">
      <div class="fp-ws">
{tiles}
      </div>
      <div class="fp-clock"><b id="fp-time">--:--</b><span id="fp-date">&#160;</span></div>
      <div class="fp-status">
        <i>{status}</i>
        <i>&copy;&nbsp;<a href="https://www.lambdabetaeta.eu">Alex Kavvos</a></i>
      </div>
    </div>"""


def menubar(current: str) -> str:
    """The site nav, generated once so every page's menubar stays in step."""
    items = "\n".join(
        f'        <a class="menu-item{" current" if key == current else ""}" '
        f'href="{href}">{label}</a>'
        for href, label, key in NAV
    )
    return (
        '      <nav class="menubar" aria-label="Site">\n'
        f'{items}\n'
        '        <a class="menu-item" href="exarch/index.html">exarch'
        '<span class="ext"> &#8599;</span></a>\n'
        '        <span class="grow"></span>\n'
        '        <button class="btn" id="theme-toggle" type="button" aria-label="Switch theme">\n'
        '          <span id="theme-glyph">&#9788;</span><span id="theme-label">Workspace</span>\n'
        '        </button>\n'
        '      </nav>'
    )


# ── shared page shell ──────────────────────────────────────────────────────

def asset_url(name: str) -> str:
    """Return a content-versioned URL for a generated site's static asset."""
    path = SITE / name
    if not path.is_file():
        raise SystemExit(f"render-site: shared asset is missing: {path}")
    version = hashlib.sha256(path.read_bytes()).hexdigest()[:12]
    return f"{name}?v={version}"


def doc_page(title: str, current: str, body: str, source: str,
             wrap: str = '<div class="wrap doc">',
             client: str = "client") -> str:
    """The reading-page shell: window chrome around a client area, styled by
    doc.css.  ``source`` is the filename reported in the status readout."""
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>ral — {title}</title>
  <link rel="icon" href="favicon.svg" type="image/svg+xml">
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600;700&family=IBM+Plex+Serif:ital,wght@0,400;0,500;0,600;1,400;1,500&display=swap" rel="stylesheet">
  <link rel="stylesheet" href="{asset_url("site.css")}">
  <link rel="stylesheet" href="{asset_url("doc.css")}">
{THEME_BOOT}
</head>
<body>
<div class="workspace">
  <div class="app">

    <div class="chrome">
      <div class="titlebar">
        <span class="ico" aria-hidden="true">!{{}}</span>
        <span class="grow">ral — {title}</span>
        <span class="pad" aria-hidden="true"></span>
      </div>
{menubar(current)}
      <div class="sep" aria-hidden="true"></div>
    </div>

    <div class="{client}">
      {wrap}
{body}
      </div>
    </div>

{front_panel(current, source)}

  </div>
</div>
{CHROME_SCRIPT}
</body>
</html>
"""


# ── highlighting ───────────────────────────────────────────────────────────

_CELL_RE = re.compile(r"<td class=line>(.*?)</td>", re.DOTALL)


def ensure_tree_sitter() -> None:
    """Generate the parser if needed and confirm the CLI runs.

    The parser is a build artefact (gitignored); `generate --js-runtime
    native` uses the bundled QuickJS runtime, so no node or bun is required.
    """
    if not (GRAMMAR_DIR / "src" / "parser.c").is_file():
        run(["tree-sitter", "generate", "--js-runtime", "native"],
            "could not generate the ral parser")
    run(["tree-sitter", "--version"], "the tree-sitter CLI is not runnable")


def run(cmd: list[str], what: str) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(cmd, cwd=GRAMMAR_DIR, check=True, timeout=60,
                              capture_output=True, text=True,
                              env={**os.environ, "TREE_SITTER_DIR": str(TS_CONFIG_DIR)})
    except (FileNotFoundError, subprocess.CalledProcessError,
            subprocess.TimeoutExpired, OSError) as exc:
        detail = getattr(exc, "stderr", "") or ""
        raise SystemExit(
            f"render-site: {what}: {exc}\n{detail}\n"
            f"Install the tree-sitter CLI — every ral listing on the site is "
            f"highlighted by the grammar, and there is no fallback."
        ) from exc


def highlight_ral(source: str) -> str:
    """Highlight ral source through the grammar, returning class-tagged HTML.

    The grammar is associated by file extension, so the source goes to a
    temporary `.ral` file.  `highlight --html` emits one table row per line;
    the highlighted code lives in the `<td class=line>` cells, which join back
    into the original source.
    """
    tmp = GRAMMAR_DIR / ".render-site.ral"
    tmp.write_text(source.rstrip("\n") + "\n", encoding="utf-8")
    try:
        out = run(["tree-sitter", "highlight", "--html", "--css-classes", str(tmp)],
                  "highlighting failed").stdout
    finally:
        tmp.unlink(missing_ok=True)
    cells = _CELL_RE.findall(out)
    if not cells:
        raise SystemExit("render-site: tree-sitter returned no highlighted lines")
    return "".join(cells).rstrip("\n")


SH_KEYWORDS = {
    "if", "then", "elif", "else", "fi", "for", "in", "do", "done", "while",
    "until", "case", "esac", "function", "return", "exit", "local", "set",
    "declare", "read", "shift", "trap", "break", "continue",
}
SH_TOKEN_RE = re.compile(
    r"""(?P<comment>\#[^\n]*)
      | (?P<sq>'[^']*')
      | (?P<dq>"(?:\\.|[^"\\])*")
      | (?P<var>\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*|\$[0-9@*\#?!$-])
      | (?P<word>[A-Za-z_][A-Za-z0-9_-]*)
      | (?P<op>[|&;<>()]+)
    """,
    re.VERBOSE,
)
SH_CLASS = {"comment": "comment", "sq": "string", "dq": "string",
            "var": "variable", "op": "operator"}


def highlight_sh(source: str) -> str:
    """Colour the bash counterpart.  There is no bash grammar vendored here,
    and the pane exists to be compared against rather than studied, so a
    tokeniser covering comments, quoting, expansion and keywords is enough."""
    out: list[str] = []
    pos = 0
    for m in SH_TOKEN_RE.finditer(source):
        out.append(html.escape(source[pos:m.start()]))
        text = html.escape(m.group())
        cls = SH_CLASS.get(m.lastgroup)
        if m.lastgroup == "word" and m.group() in SH_KEYWORDS:
            cls = "keyword"
        out.append(f'<span class="{cls}">{text}</span>' if cls else text)
        pos = m.end()
    out.append(html.escape(source[pos:]))
    return "".join(out)


# ── examples ───────────────────────────────────────────────────────────────

def split_source(text: str) -> tuple[str, str]:
    """Return (leading comment block, body) with any shebang dropped.

    The comment block is the prose the reader meets first; a blank line ends
    it.  It is authored in markdown, so it renders like the doc pages.
    """
    lines = text.splitlines()
    if lines and lines[0].startswith("#!"):
        lines = lines[1:]
    while lines and not lines[0].strip():
        lines = lines[1:]

    head: list[str] = []
    i = 0
    while i < len(lines) and lines[i].lstrip().startswith("#"):
        s = lines[i].lstrip()
        head.append("" if s == "#" else s[2:] if s.startswith("# ") else s[1:])
        i += 1
    while i < len(lines) and not lines[i].strip():
        i += 1
    return "\n".join(head).strip(), "\n".join(lines[i:]).rstrip()


def collect_examples() -> list[dict]:
    """Every example, as a pair of readings of one task.

    Each lives in its own directory holding `<stem>.ral` beside `<stem>.sh`.
    The stem is taken from the ral file rather than the directory, because a
    couple of directories name their program differently.
    """
    found: list[dict] = []
    for d in sorted(p for p in EXAMPLES_DIR.iterdir() if p.is_dir()):
        rals = sorted(d.glob("*.ral"))
        if not rals:
            raise SystemExit(f"render-site: {d} holds no .ral program")
        ral = rals[0]
        sh = ral.with_suffix(".sh")
        if not sh.is_file():
            raise SystemExit(f"render-site: {ral.name} has no .sh counterpart")
        note, ral_body = split_source(ral.read_text(encoding="utf-8"))
        task, sh_body = split_source(sh.read_text(encoding="utf-8"))
        found.append({
            "slug": d.name,
            "stem": ral.stem,
            "note": markdown.markdown(note, extensions=["extra", "sane_lists"]),
            "task": task,
            "ral_source": ral_body,
            "sh_source": sh_body,
            "ral": highlight_ral(ral_body),
            "sh": highlight_sh(sh_body),
        })
    order = {name: i for i, name in enumerate(CURATED_ORDER)}
    found.sort(key=lambda e: (order.get(e["slug"], len(CURATED_ORDER)), e["slug"]))
    return found


def pane(label: str, lang: str, code: str) -> str:
    return (f'          <div class="pane {lang}">\n'
            f'            <div class="ph">{html.escape(label)}</div>\n'
            f'            <pre><code>{code}</code></pre>\n'
            f'          </div>')


def both_readings(e: dict) -> str:
    return (pane(f'{e["stem"]}.sh — bash', "sh", e["sh"]) + "\n"
            + pane(f'{e["stem"]}.ral — ral', "ral", e["ral"]))


def source_tail(example: dict, language: str, marker: str) -> str:
    lines = example[f"{language}_source"].splitlines()
    start = next((i for i, line in enumerate(lines) if marker in line), None)
    if start is None:
        name = f'{example["stem"]}.{language}'
        raise SystemExit(
            f"render-site: landing excerpt marker {marker!r} is missing from {name}"
        )
    return "\n".join(lines[start:])


def landing_readings(example: dict, spec: dict) -> str:
    sh = source_tail(example, "sh", spec["sh_start"])
    ral = source_tail(example, "ral", spec["ral_start"])
    return (
        pane(f'{example["stem"]}.sh', "sh", highlight_sh(sh))
        + "\n"
        + pane(f'{example["stem"]}.ral', "ral", highlight_ral(ral))
    )


EXAMPLES_SCRIPT = """  <script>
    (function () {
      var list  = document.getElementById('exlist');
      var find  = document.getElementById('exfind');
      var shown = document.getElementById('exshown');
      var empty = document.getElementById('exempty');
      var opts  = [].slice.call(list.children);

      function panel(li) { return document.getElementById('ex-' + li.dataset.slug); }

      function select(li) {
        if (!li) return;
        opts.forEach(function (o) {
          var on = o === li;
          o.setAttribute('aria-selected', on ? 'true' : 'false');
          panel(o).hidden = !on;
        });
        list.setAttribute('aria-activedescendant', li.id);
        li.scrollIntoView({ block: 'nearest' });
        location.hash = li.dataset.slug;
      }

      list.addEventListener('click', function (ev) {
        var li = ev.target.closest('li');
        if (li) select(li);
      });

      list.addEventListener('keydown', function (ev) {
        var step = ev.key === 'ArrowDown' ? 1 : ev.key === 'ArrowUp' ? -1 : 0;
        if (!step) return;
        ev.preventDefault();
        var vis = opts.filter(function (o) { return !o.hidden; });
        var i = vis.indexOf(list.querySelector('[aria-selected="true"]'));
        select(vis[Math.min(vis.length - 1, Math.max(0, i + step))]);
      });

      find.addEventListener('input', function () {
        var t = find.value.trim().toLowerCase(), first = null, kept = null, n = 0;
        opts.forEach(function (o) {
          var hit = !t || o.dataset.name.indexOf(t) !== -1;
          o.hidden = !hit;
          if (hit) {
            n++;
            if (!first) first = o;
            if (o.getAttribute('aria-selected') === 'true') kept = o;
          }
        });
        shown.textContent = n;
        empty.hidden = n !== 0;
        if (!kept) {
          opts.forEach(function (o) { panel(o).hidden = true; });
          if (first) select(first);
        }
      });

      var wanted = opts.filter(function (o) {
        return o.dataset.slug === location.hash.slice(1);
      })[0];
      if (wanted) select(wanted);
    })();
  </script>"""


def render_examples(examples: list[dict]) -> None:
    items = "\n".join(
        f'            <li role="option" id="opt-{e["slug"]}" data-slug="{e["slug"]}"'
        f' data-name="{html.escape(e["slug"] + " " + e["task"].lower(), quote=True)}"'
        f' aria-selected="{"true" if i == 0 else "false"}">'
        f'<b>{html.escape(e["stem"])}</b><span>{html.escape(e["task"])}</span></li>'
        for i, e in enumerate(examples)
    )
    panels = "\n".join(
        f'          <section class="example" id="ex-{e["slug"]}"{"" if i == 0 else " hidden"}>\n'
        f'            <div class="exbar"><b>{html.escape(e["stem"])}</b>'
        f'<span>{html.escape(e["task"])}</span></div>\n'
        f'            <div class="exbody">\n'
        f'              <div class="summary">{e["note"]}</div>\n'
        f'              <div class="split stack">\n{both_readings(e)}\n              </div>\n'
        f'            </div>\n'
        f'          </section>'
        for i, e in enumerate(examples)
    )
    body = f"""        <div class="exsplit">
          <div class="exlist">
            <div class="exfind">
              <label for="exfind">Find:</label>
              <input id="exfind" type="search" placeholder="csv, parallel, hash&#8230;" autocomplete="off">
            </div>
            <ol id="exlist" role="listbox" aria-label="Examples" tabindex="0"
                aria-activedescendant="opt-{examples[0]["slug"]}">
{items}
            </ol>
            <p class="exfoot"><span id="exshown">{len(examples)}</span> of {len(examples)} examples</p>
          </div>
          <div class="exdetail">
{panels}
            <p class="exempty" id="exempty" hidden>No example matches that.</p>
          </div>
        </div>"""
    page = doc_page("examples", "examples", body, "examples/",
                    wrap='<div class="exwrap">', client="client form")
    page = page.replace("</body>", EXAMPLES_SCRIPT + "\n</body>")
    (SITE / "examples.html").write_text(page, encoding="utf-8")


# ── landing ────────────────────────────────────────────────────────────────

def render_index(examples: list[dict]) -> None:
    template = (ROOT / "scripts" / "index.template.html").read_text(encoding="utf-8")
    downloads = json.loads((SITE / "downloads.json").read_text(encoding="utf-8"))
    base = f'https://github.com/{downloads["release_repo"]}/releases/download/latest'
    # The installer column is Windows-only for now, and `installer` is absent
    # from the other targets rather than empty: an em dash is the cell for a
    # platform that has no such thing, not for one whose file we forgot.
    def cell(target: dict, key: str) -> str:
        name = target.get(key)
        if not name:
            return "<td>&mdash;</td>"
        return f'<td><a href="{base}/{name}" download>{html.escape(name)}</a></td>'

    rows = "\n".join(
        f'              <tr><td class="os">{html.escape(t["os"])}</td>'
        f'{cell(t, "primary")}{cell(t, "allutils")}{cell(t, "installer")}</tr>'
        for t in downloads["targets"]
    )
    by_slug = {example["slug"]: example for example in examples}
    missing = {
        spec["slug"] for spec in LANDING_EXAMPLES.values()
    } - by_slug.keys()
    if missing:
        absent = ", ".join(sorted(missing))
        raise SystemExit(f"render-site: landing examples missing from examples/: {absent}")

    for placeholder, value in [
        ("{{SITE_CSS}}", asset_url("site.css")),
        ("{{INDEX_CSS}}", asset_url("index.css")),
        ("{{MENUBAR}}", menubar("index")),
        ("{{DOWNLOAD_GROUPS}}", rows),
        ("{{EFFECT_EXAMPLE}}",
         landing_readings(by_slug[LANDING_EXAMPLES["effect"]["slug"]],
                          LANDING_EXAMPLES["effect"])),
        ("{{VALUE_EXAMPLE}}",
         landing_readings(by_slug[LANDING_EXAMPLES["value"]["slug"]],
                          LANDING_EXAMPLES["value"])),
        ("{{SANDBOX_EXAMPLE}}", highlight_ral(SANDBOX_EXAMPLE)),
        ("{{EXAMPLE_COUNT}}", str(len(examples))),
        ("{{FRONT_PANEL}}", front_panel("index", LICENSE_STATUS)),
    ]:
        if placeholder not in template:
            raise SystemExit(f"missing placeholder {placeholder} in index.template.html")
        template = template.replace(placeholder, value)
    template = template.replace("</body>", CHROME_SCRIPT + "\n</body>")
    (SITE / "index.html").write_text(template, encoding="utf-8")


# ── docs ───────────────────────────────────────────────────────────────────

_BLOCK_CODE_RE = re.compile(
    r'<pre><code(?: class="language-ral")?>(.*?)</code></pre>',
    re.DOTALL,
)


def highlight_doc_blocks(body: str) -> str:
    """Re-highlight every fenced code block in markdown-rendered HTML.

    Markdown emits `<pre><code>…</code></pre>` with the source HTML-escaped;
    the highlighter needs raw source and does its own escaping, so the inner
    text is unescaped first.  Inline `<code>…</code>` spans have no preceding
    `<pre>` and are left untouched.
    """
    return _BLOCK_CODE_RE.sub(
        lambda m: f"<pre><code>{highlight_ral(html.unescape(m.group(1)))}</code></pre>",
        body,
    )


def render_docs() -> None:
    for src, title, key, dst in [
        (DOCS_DIR / "TUTORIAL.md", "tutorial", "tutorial", "tutorial.html"),
        (DOCS_DIR / "SPEC.md", "specification", "spec", "spec.html"),
        (DOCS_DIR / "RATIONALE.md", "rationale", "rationale", "rationale.html"),
    ]:
        text = src.read_text(encoding="utf-8")
        # The sources link to each other by repo path; on the site those
        # documents live side by side as pages.
        text = text.replace("SPEC.md", "spec.html")
        text = text.replace("RATIONALE.md", "rationale.html")
        body = markdown.markdown(text, extensions=["extra", "sane_lists"])
        body = highlight_doc_blocks(body)
        (SITE / dst).write_text(
            doc_page(title, key, f"        <article>\n{body}\n        </article>",
                     src.name),
            encoding="utf-8")


# ── exarch sub-site ─────────────────────────────────────────────────────────

def render_exarch_downloads(downloads: dict) -> str:
    """Per-OS binary buttons for the exarch landing's get section.

    Mirrors the landing's download table but for exarch's matrix: macOS +
    Linux only, one binary per target (no `allutils` split — those utils
    belong to ral)."""
    base = f'https://github.com/{downloads["release_repo"]}/releases/download/latest'
    lines: list[str] = []
    for target in downloads["targets"]:
        os_name = html.escape(target["os"])
        artifact = html.escape(target["artifact"])
        lines.extend(
            [
                "",
                '          <div class="dl-group">',
                f'            <span class="dl-os">{os_name}</span>',
                f'            <a class="dl-btn" href="{base}/{artifact}" download>{artifact}</a>',
                "          </div>",
            ]
        )
    lines.append("")
    return "\n".join(lines)


def exarch_menubar(current: str) -> str:
    """Shared nav for the exarch sub-site; ``current`` is the active page key
    (``index`` or ``profiles``).  Generated once so both the landing template
    and the profiles shell stay in step."""
    def item(href: str, label: str, key: str, ext: bool = False) -> str:
        cls = "menu-item current" if key == current else "menu-item"
        tail = '<span class="ext"> &#8599;</span>' if ext else ""
        return f'<a class="{cls}" href="{href}">{label}{tail}</a>'

    return (
        '<nav class="menubar" aria-label="exarch">\n'
        f'    {item("index.html", "exarch", "index")}\n'
        f'    {item("profiles.html", "Profiles", "profiles")}\n'
        f'    {item("../index.html", "ral", "ral", ext=True)}\n'
        '    <span class="spacer"></span>\n'
        '    <button class="theme-toggle" id="theme-toggle" type="button" aria-label="Switch theme">\n'
        '      <span class="glyph" id="theme-glyph">&#9788;</span><span id="theme-label">light</span>\n'
        '    </button>\n'
        '  </nav>'
    )


def exarch_doc_page(title: str, body: str, titlebar: str) -> str:
    """The exarch reading-page shell: heraldic chrome styled by exarch.css +
    its own doc.css.  Mirrors doc_page() but for the exarch sub-site (own
    menubar, footer, favicon, and only IBM Plex Mono)."""
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>exarch — {title}</title>
  <link rel="icon" href="favicon.svg" type="image/svg+xml">
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600&display=swap" rel="stylesheet">
  <link rel="stylesheet" href="exarch.css">
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
  {exarch_menubar("profiles")}
  <main>
    <div class="window">
      <div class="titlebar">
        <span class="app-icon" aria-hidden="true">&#9656;</span>
        <span class="title-text">{titlebar}</span>
      </div>
      <article>
{body}
      </article>
    </div>
  </main>
  <footer class="footer">
    exarch &mdash; a tiny coding agent driving a capability-secure shell.
    &middot; <a href="index.html">home</a> &middot; <a href="../index.html">ral</a>
  </footer>
  <script>
    (function () {{
      var root  = document.documentElement;
      var btn   = document.getElementById('theme-toggle');
      var glyph = document.getElementById('theme-glyph');
      var label = document.getElementById('theme-label');
      function syncToggle() {{
        var dark = root.classList.contains('dark');
        glyph.textContent = dark ? '☾' : '☀';
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


def render_exarch() -> None:
    """Build the exarch sub-site under site/exarch/.

    The landing is a bespoke page; the build injects the shared menubar so its
    nav stays in step with the profiles page.  The profiles reference is
    generated from exarch/PROFILES.md — the single source of truth — so the
    page can never drift from the documentation.
    """
    template = (ROOT / "scripts" / "exarch-index.template.html").read_text(
        encoding="utf-8")
    for placeholder in ("{{MENUBAR}}", "{{EXARCH_DOWNLOADS}}"):
        if placeholder not in template:
            raise SystemExit(
                f"missing placeholder {placeholder} in exarch-index.template.html")
    downloads = json.loads(
        (EXARCH_SITE / "downloads.json").read_text(encoding="utf-8"))
    rendered = template.replace("{{MENUBAR}}", exarch_menubar("index"))
    rendered = rendered.replace(
        "{{EXARCH_DOWNLOADS}}", render_exarch_downloads(downloads))
    (EXARCH_SITE / "index.html").write_text(rendered, encoding="utf-8")

    profiles_md = (EXARCH_DIR / "PROFILES.md").read_text(encoding="utf-8")
    body = markdown.markdown(profiles_md, extensions=["extra", "sane_lists", "toc"])
    (EXARCH_SITE / "profiles.html").write_text(
        exarch_doc_page("capability profiles", body, titlebar="PROFILES.md"),
        encoding="utf-8")


def main() -> int:
    ensure_tree_sitter()
    examples = collect_examples()
    render_index(examples)
    render_docs()
    render_examples(examples)
    render_exarch()
    print(f"render-site: {len(examples)} examples, {len(NAV) + 1} pages")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
