#!/usr/bin/env python3
"""Re-baseline dead wiki provenance stamps after a history rewrite.

The `map/`, `internals/`, and `related/` pages carry frontmatter stamps
(`generated_at_commit` / `verified_at_commit`, each paired with a `_date`)
that drive the drift-lint described in `docs/ral-wiki/AGENTS.md`:

    git log <stamp>..HEAD -- <covers_paths>

A squash or rebase rewrites commit identity, so every stamp pointing at a
pre-rewrite commit no longer resolves and the lint command errors. This
script rewrites only the stamps whose commit no longer exists, pointing them
at a baseline that does (the repository root by default — the honest,
conservative choice: `<root>..HEAD` flags every page as "may be stale", which
is the true post-squash state, rather than falsely asserting verification at
HEAD). Stamps that still resolve are left untouched so genuine recent
verifications are preserved.

Only the leading `---` frontmatter block of each file is touched; `yaml`
examples in page bodies (e.g. in AGENTS.md) are never rewritten.

Usage:
    scripts/wiki-restamp.py [--baseline <rev>] [--check] [wiki_dir]

    --check   report what would change and exit non-zero if anything is
              dead, without writing (suitable for CI after a squash).
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

STAMP_RE = re.compile(r"^(?P<key>(?:generated|verified)_at_commit):[ \t]*(?P<hash>[0-9a-f]{7,40})[ \t]*$")
DATE_RE = re.compile(r"^(?P<key>(?:generated|verified)_at_date):[ \t]*(?P<date>\S+)[ \t]*$")


def git(*args: str) -> str:
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout.strip()


def commit_exists(rev: str) -> bool:
    return subprocess.run(["git", "cat-file", "-t", rev], capture_output=True).returncode == 0


def frontmatter_span(lines: list[str]) -> tuple[int, int] | None:
    """Return (start, end) line indices of the leading `---`..`---` block, or None."""
    if not lines or lines[0].rstrip() != "---":
        return None
    for i in range(1, len(lines)):
        if lines[i].rstrip() == "---":
            return (1, i)
    return None


def restamp_file(path: Path, base_hash: str, base_date: str, write: bool) -> list[str]:
    """Rewrite dead stamps in one file's frontmatter. Returns human-readable changes."""
    lines = path.read_text().splitlines(keepends=True)
    span = frontmatter_span(lines)
    if span is None:
        return []
    start, end = span
    changes: list[str] = []
    # Collect which stamp prefixes went dead so we can fix their paired dates.
    dead_prefixes: set[str] = set()
    for i in range(start, end):
        m = STAMP_RE.match(lines[i].rstrip("\n"))
        if not m:
            continue
        old = m.group("hash")
        if commit_exists(old):
            continue
        prefix = m.group("key").rsplit("_at_commit", 1)[0]
        dead_prefixes.add(prefix)
        lines[i] = f"{m.group('key')}: {base_hash}\n"
        changes.append(f"{m.group('key')} {old} -> {base_hash}")
    for i in range(start, end):
        m = DATE_RE.match(lines[i].rstrip("\n"))
        if not m:
            continue
        prefix = m.group("key").rsplit("_at_date", 1)[0]
        if prefix not in dead_prefixes or m.group("date") == base_date:
            continue
        lines[i] = f"{m.group('key')}: {base_date}\n"
        changes.append(f"{m.group('key')} {m.group('date')} -> {base_date}")
    if changes and write:
        path.write_text("".join(lines))
    return changes


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("wiki_dir", nargs="?", default="docs/ral-wiki", type=Path)
    ap.add_argument("--baseline", help="rev to re-stamp dead stamps to (default: repository root)")
    ap.add_argument("--check", action="store_true", help="report only; exit 1 if any dead stamp found")
    args = ap.parse_args()

    baseline = args.baseline or git("rev-list", "--max-parents=0", "HEAD").splitlines()[0]
    base_hash = git("rev-parse", "--short=7", baseline)
    base_date = git("log", "-1", "--format=%cd", "--date=short", baseline)

    total = 0
    for path in sorted(args.wiki_dir.rglob("*.md")):
        changes = restamp_file(path, base_hash, base_date, write=not args.check)
        if changes:
            total += len(changes)
            rel = path.relative_to(args.wiki_dir)
            print(f"{rel}")
            for c in changes:
                print(f"    {c}")

    verb = "would re-stamp" if args.check else "re-stamped"
    print(f"\n{verb} {total} field(s) to {base_hash} ({base_date}).", file=sys.stderr)
    return 1 if (args.check and total) else 0


if __name__ == "__main__":
    raise SystemExit(main())
