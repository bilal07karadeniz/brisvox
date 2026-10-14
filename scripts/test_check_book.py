"""Tests of scripts/check_book.py on throw-away book trees (python -m unittest, stdlib only)."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_book  # noqa: E402


def write(root: Path, rel: str, text: str = "x\n") -> None:
    p = root / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")


def good_tree(root: Path) -> None:
    write(root, "book/book.toml", '[book]\ntitle = "t"\nsrc = "src"\n')
    write(root, "book/src/SUMMARY.md", "# Summary\n\n- [Intro](intro.md)\n- [Ref](ref.md)\n")
    write(root, "book/src/intro.md", "# Intro\n\nSee [ref](ref.md#a) and [web](https://x.y/z).\n")
    write(root, "book/src/ref.md", "{{#include ../../docs/A.md}}\n")
    write(root, "docs/A.md", "# A\n")
    write(root, "docs/providers/_TEMPLATE.md", "# tpl\n")
    write(root, "README.md", "[book](book/src/intro.md)\n")


class CheckBook(unittest.TestCase):
    def run_check(self, mutate=None):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            good_tree(root)
            if mutate:
                mutate(root)
            return check_book.check(root)

    def test_good_tree_passes(self):
        self.assertEqual(self.run_check(), [])

    def test_missing_summary_target(self):
        def m(r):
            write(r, "book/src/SUMMARY.md", "- [Intro](intro.md)\n- [Ref](ref.md)\n- [Gone](gone.md)\n")

        self.assertTrue(any("gone.md" in p for p in self.run_check(m)))

    def test_draft_chapter(self):
        def m(r):
            write(r, "book/src/SUMMARY.md", "- [Intro](intro.md)\n- [Ref](ref.md)\n- [Draft]()\n")

        self.assertTrue(any("draft" in p for p in self.run_check(m)))

    def test_duplicate_chapter(self):
        def m(r):
            write(r, "book/src/SUMMARY.md", "- [Intro](intro.md)\n- [Ref](ref.md)\n- [Again](intro.md)\n")

        self.assertTrue(any("listed twice" in p for p in self.run_check(m)))

    def test_unlisted_chapter_file(self):
        self.assertTrue(any("not listed" in p for p in self.run_check(lambda r: write(r, "book/src/extra.md"))))

    def test_broken_include(self):
        def m(r):
            write(r, "book/src/ref.md", "{{#include ../../docs/A.md}}\n{{#include ../../docs/NOPE.md}}\n")

        self.assertTrue(any("NOPE.md" in p for p in self.run_check(m)))

    def test_include_anchor_is_stripped(self):
        def m(r):
            write(r, "book/src/ref.md", "{{#include ../../docs/A.md:anchor}}\n")

        self.assertEqual(self.run_check(m), [])

    def test_broken_link(self):
        def m(r):
            write(r, "book/src/intro.md", "[x](nowhere.md)\n")

        self.assertTrue(any("nowhere.md" in p for p in self.run_check(m)))

    def test_links_in_code_are_ignored(self):
        def m(r):
            write(r, "book/src/intro.md", "`[x](nowhere.md)`\n```\n[y](nowhere2.md)\n```\n")

        self.assertEqual(self.run_check(m), [])

    def test_doc_not_in_book(self):
        self.assertTrue(any("B.md" in p for p in self.run_check(lambda r: write(r, "docs/B.md"))))

    def test_provider_doc_not_in_book(self):
        self.assertTrue(any("x.md" in p for p in self.run_check(lambda r: write(r, "docs/providers/x.md"))))

    def test_readme_broken_link(self):
        self.assertTrue(any("README" in p for p in self.run_check(lambda r: write(r, "README.md", "[x](gone/y.md)\n"))))

    def test_missing_book_toml(self):
        def m(r):
            (r / "book/book.toml").unlink()

        self.assertTrue(any("book.toml" in p for p in self.run_check(m)))


class RealRepo(unittest.TestCase):
    def test_the_repository_book_is_consistent(self):
        root = Path(__file__).resolve().parent.parent
        self.assertEqual(check_book.check(root), [])


if __name__ == "__main__":
    unittest.main()
