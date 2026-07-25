# The ral/exarch wiki — maintainer contract

This directory is a persistent, interlinked knowledge base for **ral** (the
language and runtime) and **exarch** (the LLM agent embedding it). You — the
LLM — write and maintain every page here. The human curates sources, directs
the analysis, and asks questions. You do the bookkeeping.

This file is the schema: it tells you how the wiki is structured and what to do
when ingesting a change, answering a question, or linting. Keep it current as
the conventions co-evolve.

The wiki lives at `docs/ral-wiki/` in the public ral repository. Browse it
locally by opening this directory as an Obsidian vault — wikilinks, graph view,
backlinks, and search work directly, no build step.

## What this wiki is for

It captures the **durable** knowledge that the code itself cannot hold: *why*
the design is the way it is, the invariants it must preserve, the decisions
that produced it, and a thin map of *where* things live. It is not a mirror of
the source — `docs/SPEC.md` is the formal published reference, and the code is
the truth. The wiki links to both; it never copies them.

## Layers

The wiki has five layers, each with a different relationship to staleness.

**Durable — `design/` and `invariants/`.** The *why*. Theory pages (call-by-
push-value, algebraic effects, deep self-masking handlers, row types, the
capability calculus) and cross-cutting concept pages that span ral and exarch
(e.g. `grant` as the boundary that exarch reuses as its sandbox). Invariants are
short pages stating one hard rule each. These do not go stale when code is
refactored — they change only when a decision changes them, and then you record
the supersession in `decisions/`. No staleness stamp.

**Chronological — `decisions/`.** The *why we changed*. One ADR-style page per
significant decision, named `YYMMDD_slug.md`. Frontmatter carries
`status: proposed | active | fixed | superseded | rejected | open`. When a
decision is superseded, set its status and link forward; never delete the old
page and never rewrite history into it.

**Volatile — `map/`.** The *where*. Thin per-subsystem pages (the core engine,
the exarch attend loop, the capability sandbox, the REPL, the prelude). A map page
points at files and symbols and links out to the durable pages; it does **not**
reproduce code. Every map page carries frontmatter:

```yaml
---
generated_at_commit: 88e94f0
generated_at_date: 2026-05-30
covers_paths: [exarch/]
---
```

This stamp is what keeps the volatile layer honest — see Lint.

**Operational — `internals/`.** The *how it runs*. A small, curated set of
narratives that thread several subsystems into one flow — the compilation ladder
from source to typed IR, the evaluator as a trampolined CBPV machine, the
lifecycle of one run. These are *semi-durable*: the high-level machine outlives
file moves and renames, but a deep architectural decision can rewrite it. An
internals page explains *how a behaviour is realised* in terms stable across
refactors; it links **down** to `map/` for the symbols that realise it and
**up** to `design/` for why. The test: if a claim's content *is* a particular
file or symbol, it belongs in `map/`; if it argues a choice, in `design/`; if it
narrates how the parts move at runtime, here. Keep this layer curated like
`design/` — one page per *flow or mechanism*: a runtime flow that crosses
subsystems, or an algorithm whose *how* neither `map/` (which points at code) nor
`design/` (which argues the choice) carries. Never a file-by-file restatement of
a single `map/` page. Each page carries a light stamp:

```yaml
---
verified_at_commit: 8cd5879
verified_at_date: 2026-05-31
anchors: [eval_top_level, Settled<Value>, trampoline, Mobile]
---
```

`anchors` are the load-bearing symbols or invariants the narrative depends on.
They are the drift signal — coarser than map's `covers_paths`, and semantic
rather than mechanical: a file move alone does not stale the page, a vanished
anchor or a superseding decision does (see Lint).

**Comparative — `related/`.** The *how it compares*. One page per published
system or tight cluster (a calculus, a language, a body of work), read against
ral's own pages: what corresponds, what deliberately diverges, what ral could
borrow. Durable on the external work — a published calculus does not change — but
keyed to ral's design via a stamp naming the pages it rests on:

```yaml
---
verified_at_commit: 2c7c523
verified_at_date: 2026-06-03
against: [design/grant, design/effects-handlers, design/cbpv]
---
```

`against` lists the ral pages the comparison leans on. It is the drift signal: a
file move never flags the page, but a `decisions/` page that supersedes a
compared-against design page does (see Lint). These pages are seeded by a query
that reads ral against a source the human is studying — the comparative twin of
the `design/` exploration that an ingest files back.

## Navigation files

**`index.md`** — content catalog, and the only whole-corpus view: the layers,
what's covered, where the gaps are. Every page listed once under its category
with a one-line thesis. Read this first when answering a query, then drill
into pages. It carries **no stamps** — provenance lives in page frontmatter,
where the drift lint reads it — so an ingest touches the index only when a
page's *thesis* changes (or a page is added or superseded), not on every
restamp.

There is no separate timeline file: git history is the timeline (each sync
commits with a message naming the pass), and `decisions/` carries the "why it
changed" story.

## Searching the wiki (qmd)

The wiki has its own **project-local** retrieval index, built by **qmd** — an
on-device hybrid search engine (BM25 + local vector embeddings + reranking, all
offline). The index lives at `<repo>/.qmd/` and covers only this wiki: qmd finds
it by walking up from its working directory for `.qmd/index.yaml` (the way git
finds `.git`), and the qmd MCP server runs with the repo root as its cwd, so it
serves this index and no other. A search from here can only ever return wiki
documents — isolation is structural, not a matter of scoping.

**Use it as the first-pass retrieval over the wiki:** ranked lexical and semantic
search lands on the right page faster than scanning `index.md`, and surfaces
pages whose one-line summary doesn't obviously match the question. Reach it
through the qmd MCP tools:

- **`mcp__qmd__query`** — the main entry. Combine typed sub-queries for recall —
  `lex` (BM25 keywords) for known terms and symbols (`grant`, `unify_row`,
  `Settled<Value>`), `vec` (a natural-language question) for concepts, both
  together when unsure. The first sub-query carries double weight, so lead with
  the strongest signal. No `collections` filter is needed — the index holds only
  this wiki.
- **`mcp__qmd__get`** — read an indexed page by path or `#docid` from a result;
  a line-range suffix (`map/core-engine.md:80:60`) reads context around a hit.
- **`mcp__qmd__multi_get`** — batch-fetch by glob (`design/*.md`) or a list.
- **`mcp__qmd__status`** — index health and document count.

**The index is a generated snapshot — `.qmd/` is gitignored and there is no
file-watcher.** After an ingest changes pages, rebuild it so retrieval reflects
the new state, in the same commit as every other bookkeeping step:

```bash
./scripts/wiki-index.sh
```

The script regenerates `.qmd/index.yaml` and runs `qmd update` (re-scans the
markdown, cheap) then `qmd embed` (re-vectorises only the changed chunks). The
MCP server reads the index when it starts, so a rebuild is picked up by the next
editor session.

## Linking

Use Obsidian wikilinks. Always give a path link a display alias so reading view
shows a clean name rather than a slash-path: write `[[design/cbpv|cbpv]]`, not
`[[design/cbpv]]`; for a dated decision strip the prefix, `[[decisions/260514_completion-escape-refactor|completion-escape-refactor]]`.
A bare same-folder link like `[[fixed-arity]]` needs no alias. Cross-link ral and
exarch aggressively — the connections are the point (an exarch sandbox page must
link the ral `grant` page). Link to `docs/SPEC.md` sections and to source paths
rather than restating them.

## Operations

**Ingest.** The trigger is a landed change or a design conversation, not a
dropped file. When a commit/PR lands or a decision is reached:
1. Read the diff (or the conclusion of the conversation).
2. Update the `map/` pages whose `covers_paths` the change touched; re-stamp them.
3. If a decision was made, add or supersede a `decisions/` page.
4. If an invariant was discovered or changed, add/edit an `invariants/` page.
5. If the change alters a runtime flow or the machine model an `internals/` page
   narrates, update that page and re-verify its `anchors`.
6. Update `index.md` where a page's thesis changed or a page was added.
7. Rebuild the qmd index — `./scripts/wiki-index.sh` (see Search) — so retrieval
   reflects the new pages.
Fold this into the existing "commit after every parcel of work" habit — the
wiki update belongs in the same commit as the change it describes.

A page can't stamp the commit it ships in (the hash isn't known yet, and
amending changes it). In a single commit, stamp the **parent** hash — one
stale, by design. Don't chase your own hash with a second commit or an amend.

**Query.** Retrieve with qmd (see Search) or read `index.md`, drill into the
relevant pages, synthesise an answer with `[[wikilink]]` citations. A good answer that is worth keeping is filed back
as a new `design/` page — explorations compound like ingests do. `design/` is
the home for cross-cutting concept pages too (e.g. [[design/grant|grant]],
[[design/exarch-architecture|exarch-architecture]]); there is no separate
`concepts/` folder.

**Lint.** Periodically health-check:
- *Map drift (mechanical):* for each `map/` page, run
  `git log <generated_at_commit>..HEAD -- <covers_paths>`. Non-empty output means
  the page may be stale; re-ingest it. Never silently trust an unstamped or
  drifted map page.
- *Internals drift (semantic):* for each `internals/` page, confirm its
  `anchors` still exist in the code and that no `decisions/` page supersedes the
  narrative. A file move alone does not flag the page — a vanished anchor or a
  superseding decision does. On confirmation, bump `verified_at_commit`.
- *Related drift (semantic):* for each `related/` page, confirm no `decisions/`
  page supersedes a design page in its `against` list. On confirmation, bump
  `verified_at_commit`.
- *Contradictions:* claims that disagree across pages.
- *Orphans:* pages with no inbound links; missing cross-references.
- *Gaps:* concepts referenced but lacking a page.

## House style

Honour the repository's existing authoring rules, which apply here too:
- Describe the design in **theoretical language** (CBPV / PL / effects
  vocabulary), as factual present-tense statements — not "fast path / jacket /
  lock-step" jargon.
- **No archeology.** Describe only what is true now. No tombstones, no
  "formerly X", no contrast with the old shape. Supersession lives in
  `decisions/` frontmatter, not in prose scattered across pages.
- Prefer short, poetic pages over exhaustive ones. A page earns its length.
- **Lead with the thesis, then bullet the structure.** Open a page or section with
  the organising idea in one sentence; set the load-bearing claim in bold,
  italicise a technical term on first use, then enumerate the parts as bullets
  rather than a wall of prose. Brevity and detail together — each bullet one
  precise claim, not a paragraph. This is the maintainer's prose register.
