#!/usr/bin/env python3
"""Analyse a samply-recorded Gecko-format profile.

Samply (https://github.com/mstange/samply) saves a Firefox-Profiler-format
JSON.gz that holds raw addresses; symbol resolution happens lazily in the
browser UI.  For headless triage we resolve via `atos` against the binary
that produced the profile, then print top leaf samples (where CPU actually
sits) and top inclusive samples (any frame in stack).

Usage:
    samply record --save-only -o profile.json.gz -- ./target/profiling/ral …
    python3 scripts/profiling/analyze_profile.py profile.json.gz ./target/profiling/ral

Notes:
- Uses 0x100000000 as the binary slide for atos.  This is samply's normalised
  module base; if a future samply changes that, pass --base.
- The `filter` substrings select inclusive frames worth printing.  Edit if
  your hot spots live in different module names.
"""

import argparse
import collections
import gzip
import json
import subprocess
import sys


def resolve(binary: str, raw_addrs: list[str], base: int) -> dict[str, str]:
    """Batch-resolve hex addresses to symbols via atos."""
    if not raw_addrs:
        return {}
    inputs = "\n".join(f"0x{base + int(a, 16):x}" for a in raw_addrs)
    out = subprocess.run(
        ["atos", "-o", binary, "-l", f"0x{base:x}"],
        input=inputs,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return dict(zip(raw_addrs, out.strip().split("\n")))


def load_profile(path: str) -> dict:
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt") as fh:
        return json.load(fh)


def collect_addresses(profile: dict) -> list[str]:
    addrs: set[str] = set()
    for thread in profile["threads"]:
        for name_idx in thread["funcTable"]["name"]:
            s = thread["stringArray"][name_idx]
            if s.startswith("0x"):
                addrs.add(s)
    return sorted(addrs)


def count_samples(
    profile: dict, resolved: dict[str, str]
) -> tuple[collections.Counter, collections.Counter, int]:
    leaf = collections.Counter()
    inclusive = collections.Counter()
    total = 0
    for thread in profile["threads"]:
        funcs = thread["funcTable"]
        frames = thread["frameTable"]
        stacks = thread["stackTable"]
        strs = thread["stringArray"]
        samples = thread["samples"]

        def name_of(stack_idx: int) -> str:
            frame_idx = stacks["frame"][stack_idx]
            func_idx = frames["func"][frame_idx]
            raw = strs[funcs["name"][func_idx]]
            return resolved.get(raw, raw)

        for stack_idx in samples["stack"]:
            if stack_idx is None:
                continue
            total += 1
            leaf[name_of(stack_idx)] += 1
            seen = set()
            cur = stack_idx
            while cur is not None:
                seen.add(name_of(cur))
                cur = stacks["prefix"][cur]
            for n in seen:
                inclusive[n] += 1
    return leaf, inclusive, total


def print_top(label: str, counter: collections.Counter, total: int, limit: int, *, min_pct: float = 0.0, filters: list[str] | None = None):
    print(f"=== {label} ===")
    shown = 0
    for sym, cnt in counter.most_common():
        pct = 100 * cnt / total if total else 0.0
        if pct < min_pct:
            break
        if filters and not any(f in sym for f in filters):
            continue
        print(f"{pct:5.1f}%  {cnt:5d}  {sym[:140]}")
        shown += 1
        if shown >= limit:
            break
    print()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("profile", help="samply profile.json[.gz]")
    ap.add_argument("binary", help="binary that produced the profile (for symbol resolution)")
    ap.add_argument("--base", default="0x100000000", help="binary load base for atos (default: 0x100000000)")
    ap.add_argument("--leaf", type=int, default=20, help="how many leaf entries to show")
    ap.add_argument("--inclusive", type=int, default=25, help="how many inclusive entries to show")
    ap.add_argument("--inclusive-min", type=float, default=5.0, help="inclusive %% floor for printing")
    ap.add_argument(
        "--filter",
        action="append",
        default=None,
        help="substring filter for inclusive view; repeatable. Defaults to ral_core/Shell/Env/Vec/HashMap/Arc/imbl.",
    )
    args = ap.parse_args()

    profile = load_profile(args.profile)
    raw = collect_addresses(profile)
    resolved = resolve(args.binary, raw, int(args.base, 16))

    leaf, inclusive, total = count_samples(profile, resolved)

    filters = args.filter or [
        "ral_core",
        "Shell",
        "Env",
        "with_child",
        "imbl",
        "Vec",
        "HashMap",
        "Arc",
    ]

    print(f"Samples: {total}\n")
    print_top("Top LEAF (where CPU spends cycles)", leaf, total, args.leaf)
    print_top(
        "Top INCLUSIVE (any frame on stack)",
        inclusive,
        total,
        args.inclusive,
        min_pct=args.inclusive_min,
        filters=filters,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
