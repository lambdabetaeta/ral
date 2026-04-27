#!/usr/bin/env python3
"""Render site/index.html from site/index.template.html + site/downloads.json."""

from __future__ import annotations

import html
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEMPLATE_PATH = ROOT / "site" / "index.template.html"
DOWNLOADS_PATH = ROOT / "site" / "downloads.json"
OUTPUT_PATH = ROOT / "site" / "index.html"
PLACEHOLDER = "{{DOWNLOAD_GROUPS}}"


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


def main() -> int:
    template = TEMPLATE_PATH.read_text(encoding="utf-8")
    downloads = json.loads(DOWNLOADS_PATH.read_text(encoding="utf-8"))
    if PLACEHOLDER not in template:
        raise SystemExit(f"missing placeholder {PLACEHOLDER} in {TEMPLATE_PATH}")

    rendered = template.replace(PLACEHOLDER, render_download_groups(downloads))
    OUTPUT_PATH.write_text(rendered, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
