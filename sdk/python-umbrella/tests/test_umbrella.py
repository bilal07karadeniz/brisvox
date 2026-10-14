"""The `brisvox` package is `brisvox_agent` under the short name: same API, same version."""

from __future__ import annotations

import re
import unittest
from pathlib import Path

import brisvox
import brisvox_agent


class UmbrellaTest(unittest.TestCase):
    def test_every_public_name_is_the_sdk_object(self) -> None:
        names = [n for n in brisvox_agent.__all__ if n != "__version__"]
        self.assertTrue(names)
        for name in names:
            self.assertIs(getattr(brisvox, name), getattr(brisvox_agent, name), name)
        self.assertEqual(set(brisvox.__all__), set(brisvox_agent.__all__) | {"__version__"})

    def test_star_import_works(self) -> None:
        scope: dict[str, object] = {}
        exec("from brisvox import *", scope)  # noqa: S102
        self.assertIs(scope["Agent"], brisvox_agent.Agent)

    def test_versions_agree(self) -> None:
        self.assertEqual(brisvox.__version__, brisvox_agent.__version__)
        pyproject = (Path(__file__).resolve().parents[1] / "pyproject.toml").read_text(encoding="utf-8")
        version = re.search(r'^version = "([^"]+)"', pyproject, re.M)
        self.assertIsNotNone(version)
        assert version is not None
        self.assertEqual(version.group(1), brisvox.__version__)
        # The dependency pins the SDK of the same release exactly.
        self.assertIn(f'"brisvox-agent=={brisvox.__version__}"', pyproject)

    def test_the_readme_example_builds(self) -> None:
        agent = brisvox.Agent("luigi", system_prompt="You are Luigi's host.", greeting="Hi!")

        @agent.tool
        async def opening_hours(day: str) -> str:
            """Opening hours of the restaurant on a weekday."""
            return "12:00 to 22:00"

        self.assertIsNotNone(opening_hours)


if __name__ == "__main__":
    unittest.main()
