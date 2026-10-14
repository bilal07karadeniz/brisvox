#!/usr/bin/env python3
"""Structure check of the mdBook in book/, for machines without `mdbook`.

Checks, with the standard library only:
  * book/book.toml exists and names an existing `src` directory with a SUMMARY.md;
  * every link in SUMMARY.md resolves to a file under src, none is listed twice, none is a
    draft chapter, and every other .md under src is listed;
  * every `{{#include ...}}` (also rustdoc_include / playground / embed) in a chapter resolves
    to an existing file (the `:anchor` / `:from:to` suffix is stripped);
  * every relative markdown link in a chapter (outside code) resolves to an existing file;
  * every docs/*.md and docs/providers/*.md (except `_TEMPLATE.md`) is included by some chapter,
    so the book stays the one place that organises the docs;
  * the relative links of the top-level README.md resolve.

It does not render anything: `mdbook build book` is the real build (CI / a machine with mdbook).
Exit status 0 when all checks pass, 1 otherwise (problems on stderr).

Usage: scripts/check_book.py [REPO_ROOT]
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

INCLUDE_RE = re.compile(r"\{\{#(?:include|rustdoc_include|playground|embed)\s+([^}\s]+)[^}]*\}\}")
LINK_RE = re.compile(r"(?<!\!)\[(?:[^\]]*)\]\(([^)\s]*)(?:\s+\"[^\"]*\")?\)")
FENCE_RE = re.compile(r"^\s*(```|~~~)")
# docs that are templates, not content.
DOC_EXEMPT = {"_TEMPLATE.md"}


def strip_code(text: str) -> str:
    """Text without fenced code blocks and inline code spans (links inside them are not links)."""
    out: list[str] = []
    fence = None
    for line in text.splitlines():
        m = FENCE_RE.match(line)
        if m:
            if fence is None:
                fence = m.group(1)
            elif m.group(1) == fence:
                fence = None
            continue
        if fence is None:
            out.append(re.sub(r"`[^`]*`", "", line))
    return "\n".join(out)


def is_external(target: str) -> bool:
    return bool(re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", target)) or target.startswith("//")


def relative_links(text: str) -> list[str]:
    """Relative link targets of `text` without #fragment; pure fragments and URLs are skipped."""
    found = []
    for target in LINK_RE.findall(strip_code(text)):
        target = target.strip("<>")
        if not target or target.startswith("#") or is_external(target):
            continue
        found.append(target.split("#", 1)[0].split("?", 1)[0])
    return [t for t in found if t]


def check(root: Path) -> list[str]:
    """All problems of the book under `root` (empty when it is consistent)."""
    problems: list[str] = []
    book = root / "book"
    toml = book / "book.toml"
    if not toml.is_file():
        return [f"{toml}: missing"]
    toml_text = toml.read_text(encoding="utf-8")
    if "[book]" not in toml_text:
        problems.append(f"{toml}: no [book] table")
    m = re.search(r'^\s*src\s*=\s*"([^"]+)"', toml_text, re.M)
    src = book / (m.group(1) if m else "src")
    summary = src / "SUMMARY.md"
    if not summary.is_file():
        return problems + [f"{summary}: missing"]

    # SUMMARY.md: links resolve, no duplicates, no drafts.
    listed: dict[Path, int] = {}
    summary_text = summary.read_text(encoding="utf-8")
    for lineno, line in enumerate(summary_text.splitlines(), 1):
        for title, target in re.findall(r"\[([^\]]*)\]\(([^)]*)\)", line):
            target = target.strip()
            if not target:
                problems.append(f"{summary}:{lineno}: draft chapter [{title}]() (no file)")
                continue
            if is_external(target):
                continue
            path = (src / target).resolve()
            if not path.is_file():
                problems.append(f"{summary}:{lineno}: [{title}]({target}) does not exist")
                continue
            if path in listed:
                problems.append(f"{summary}:{lineno}: {target} is listed twice (first at line {listed[path]})")
            listed.setdefault(path, lineno)
    if not listed:
        problems.append(f"{summary}: lists no chapters")

    # Every chapter file is listed; includes and links resolve; collect what is included.
    included: set[Path] = set()
    for md in sorted(src.rglob("*.md")):
        if md.resolve() == summary.resolve():
            continue
        if md.resolve() not in listed:
            problems.append(f"{md.relative_to(root)}: not listed in SUMMARY.md")
        text = md.read_text(encoding="utf-8")
        for spec in INCLUDE_RE.findall(text):
            target = spec.split(":", 1)[0]
            path = (md.parent / target).resolve()
            if not path.is_file():
                problems.append(f"{md.relative_to(root)}: include {target} does not exist")
            else:
                included.add(path)
        for target in relative_links(INCLUDE_RE.sub("", text)):
            if not (md.parent / target).resolve().exists():
                problems.append(f"{md.relative_to(root)}: link {target} does not exist")

    # The docs are all in the book.
    docs = root / "docs"
    if docs.is_dir():
        for doc in sorted(list(docs.glob("*.md")) + list((docs / "providers").glob("*.md"))):
            if doc.name in DOC_EXEMPT:
                continue
            if doc.resolve() not in included:
                problems.append(f"{doc.relative_to(root)}: not included by any chapter of the book")

    # README.md links.
    readme = root / "README.md"
    if readme.is_file():
        for target in relative_links(readme.read_text(encoding="utf-8")):
            if not (root / target).resolve().exists():
                problems.append(f"README.md: link {target} does not exist")
    else:
        problems.append("README.md: missing")
    return problems


def main(argv: list[str]) -> int:
    root = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parent.parent
    problems = check(root.resolve())
    for p in problems:
        print(f"check_book: {p}", file=sys.stderr)
    if problems:
        print(f"check_book: {len(problems)} problem(s)", file=sys.stderr)
        return 1
    print("check_book: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
