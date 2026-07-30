#!/usr/bin/env python3
"""Rebuild the self-contained HTML viewers in this folder from their markdown.

Each HTML is a self-contained viewer; the markdown source is embedded between
    <script type="text/plain" id="md-src"> ... </script>
and everything else (TOC, scroll-spy, stats, mermaid diagrams) is generated
client-side from that block. This script just splices the current markdown
back in. Each HTML carries its own header/stats branding; this script only
touches the embedded markdown.

Usage:
    python3 render.py            # render every known doc
    python3 render.py comms      # render one doc by stem or alias
"""
import re
import sys
from pathlib import Path

DOCS = Path(__file__).resolve().parent

# doc alias -> (markdown, html)
KNOWN = {
    "neo":      ("neo-jasp.md",                   "neo-jasp.html"),
    "arch":     ("architecture-refactor-plan.md", "architecture-refactor-plan.html"),
    "comms":    ("comms-protocol-spec.md",        "comms-protocol-spec.html"),
}
MARKER = '<script type="text/plain" id="md-src">'


def render_one(md_path: Path, html_path: Path) -> None:
    for p in (md_path, html_path):
        if not p.exists():
            print(f"error: missing {p}", file=sys.stderr)
            sys.exit(1)

    md = md_path.read_text(encoding="utf-8")
    # The H1 title is rendered separately in the page header — drop it here.
    md = re.sub(r"^#[^\n]*\n+", "", md, count=1)
    if "</script" in md.lower():
        print("error: markdown contains '</script' — cannot embed safely", file=sys.stderr)
        return 1

    html = html_path.read_text(encoding="utf-8")
    try:
        start = html.index(MARKER) + len(MARKER)
        end = html.index("</script>", start)
    except ValueError:
        print(f"error: embed marker not found in {html_path.name} — template damaged?", file=sys.stderr)
        sys.exit(1)

    html_path.write_text(html[:start] + "\n" + md.rstrip("\n") + "\n" + html[end:], encoding="utf-8")

    sections = len(re.findall(r"^## ", md, re.M))
    diagrams = md.count("```mermaid")
    tables = len(re.findall(r"(?m)^\|.*\|\n\|[-|: ]+\|", md))
    print(f"rendered {md_path.name} -> {html_path.name}")
    print(f"  {len(md.splitlines())} lines, {sections} sections, {diagrams} diagrams, {tables} tables")


def main() -> int:
    args = sys.argv[1:]
    if not args:
        targets = list(KNOWN.values())
    else:
        targets = []
        for a in args:
            key = a if a in KNOWN else Path(a).stem
            match = KNOWN.get(key) or next(
                (v for v in KNOWN.values() if key in (Path(v[0]).stem, Path(v[1]).stem)), None)
            if not match:
                print(f"error: unknown doc {a!r} (known: {', '.join(KNOWN)})", file=sys.stderr)
                return 1
            targets.append(match)

    for md_name, html_name in targets:
        render_one(DOCS / md_name, DOCS / html_name)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
