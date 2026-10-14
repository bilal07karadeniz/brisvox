"""Static checks of the two browser pages (no browser, no server):

    python3 -m unittest discover -s examples/web-assistant -p 'test_*.py'

The web assistant (``web/index.html``) and the transport's built-in demo page
(``crates/brisvox-transport/src/webrtc/demo.html``) are thin pages around the shared browser
module ``sdk/web/brisvox-call.js``: the call (WebRTC, the WebSocket fallback, microphone, playback,
barge-in) lives there once, tested under Node in ``sdk/web/test``. The pages must stay free of
frameworks and CDNs, import only that module, look up only elements they contain, hold no copy of
the audio code, and their script must parse (checked with ``node --check`` when Node is on PATH).
"""

from __future__ import annotations

import re
import shutil
import subprocess
import tempfile
import unittest
from html.parser import HTMLParser
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
PAGE = HERE / "web" / "index.html"
DEMO = ROOT / "crates" / "brisvox-transport" / "src" / "webrtc" / "demo.html"
MODULE = ROOT / "sdk" / "web" / "brisvox-call.js"


class Collect(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.ids: set[str] = set()
        self.urls: list[tuple[str, str]] = []
        self.scripts: list[tuple[dict[str, str | None], str]] = []
        self._attrs: dict[str, str | None] = {}
        self._in_script = False
        self._buffer: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        for name, value in attrs:
            if name == "id" and value:
                self.ids.add(value)
            if name in ("src", "href") and value:
                self.urls.append((tag, value))
        if tag == "script":
            self._in_script = True
            self._attrs = dict(attrs)
            self._buffer = []

    def handle_endtag(self, tag: str) -> None:
        if tag == "script":
            self._in_script = False
            self.scripts.append((self._attrs, "".join(self._buffer)))

    def handle_data(self, data: str) -> None:
        if self._in_script:
            self._buffer.append(data)


class Pages(unittest.TestCase):
    pages = {"web assistant": PAGE, "demo": DEMO}

    def each(self):
        for name, path in self.pages.items():
            text = path.read_text(encoding="utf-8")
            page = Collect()
            page.feed(text)
            with self.subTest(page=name):
                yield name, text, page

    def test_one_module_script_and_nothing_loaded_from_elsewhere(self) -> None:
        for _, text, page in self.each():
            self.assertEqual(len(page.scripts), 1)
            attrs, _ = page.scripts[0]
            self.assertEqual(attrs.get("type"), "module")
            self.assertNotIn("src", attrs)
            self.assertEqual(page.urls, [], "the page loads nothing: no framework, no CDN, no fonts")
            self.assertNotRegex(text, r"(?i)(?:src|href)\s*=\s*[\"']https?://")
            self.assertNotRegex(text, r"(?i)@import|<link\b")

    def test_the_only_import_is_the_shared_module(self) -> None:
        index = PAGE.read_text(encoding="utf-8")
        self.assertEqual(
            re.findall(r"^import .* from \"([^\"]+)\";", index, re.M),
            ["../../../sdk/web/brisvox-call.js"],
        )
        self.assertTrue((PAGE.parent / "../../../sdk/web/brisvox-call.js").resolve().is_file())
        demo = DEMO.read_text(encoding="utf-8")
        # The server substitutes the path of the module it serves next to the page.
        self.assertEqual(re.findall(r"^import .* from \"([^\"]+)\";", demo, re.M), ["__MODULE_PATH__"])

    def test_no_call_code_is_duplicated_in_the_pages(self) -> None:
        for _, _, page in self.each():
            script = page.scripts[0][1]
            for needle in (
                "RTCPeerConnection",
                "getUserMedia",
                "AudioWorklet",
                "WebSocket(",
                "createOffer",
                "application/sdp",
                "registerProcessor",
                "new Blob(",
            ):
                self.assertNotIn(needle, script)

    def test_every_element_the_script_looks_up_exists(self) -> None:
        for _, _, page in self.each():
            script = page.scripts[0][1]
            wanted = set(re.findall(r"\$\(\"([A-Za-z]+)\"\)", script))
            self.assertEqual(wanted - page.ids, set())
            self.assertTrue({"start", "stop", "remote", "token"} <= wanted)
            # A level meter for the microphone and one for the assistant, whichever transport.
            self.assertTrue(page.ids & {"micLevel", "level"})
            self.assertIn("botLevel", page.ids)

    def test_the_web_assistant_has_its_controls(self) -> None:
        page = Collect()
        page.feed(PAGE.read_text(encoding="utf-8"))
        wanted = set(re.findall(r"\$\(\"([A-Za-z]+)\"\)", page.scripts[0][1]))
        self.assertTrue({"start", "stop", "mute", "server", "token", "remote", "micLevel", "botLevel"} <= wanted)

    def test_the_pages_use_the_module_the_documented_way(self) -> None:
        for _, _, page in self.each():
            script = page.scripts[0][1]
            self.assertIn("new BrisvoxCall({", script)
            self.assertIn("tokenProvider:", script)
            self.assertIn("fallbackAfterMs: FALLBACK_MS", script)
            self.assertIn("|| 4000", script)  # default 4 s
            self.assertIn("audioElement: $(\"remote\")", script)
            self.assertIn("call.getLevels()", script)
            self.assertIn("call.hangup()", script)
            # Testing switches.
            self.assertIn('query.get("force")', script)
            self.assertIn('FORCE === "ws" ? "websocket"', script)
            self.assertIn('FORCE === "webrtc" ? "webrtc"', script)
            self.assertIn('query.get("fallback_ms")', script)

    def test_the_status_names_the_transport(self) -> None:
        for _, _, page in self.each():
            script = page.scripts[0][1]
            self.assertIn("`Connected (${via})`", script)
            self.assertIn('"WebRTC" : "WebSocket fallback"', script)

    def test_the_websocket_url_is_the_servers_or_the_pages_origin(self) -> None:
        index = PAGE.read_text(encoding="utf-8")
        self.assertIn("serverUrl: serverEl.value.trim()", index)
        demo = DEMO.read_text(encoding="utf-8")
        self.assertIn('const WS = "__WS_PATH__"', demo)
        self.assertIn('const WHIP = "__WHIP_PATH__"', demo)
        self.assertIn("serverUrl: location.origin", demo)
        self.assertIn("wsPath: WS || null", demo)

    def test_a_session_token_is_kept_until_a_call_connects(self) -> None:
        # The token is read again for the WebSocket fallback (the server gives a session token back
        # for a WebRTC session that never connected), so it is cleared on connect only.
        for _, _, page in self.each():
            script = page.scripts[0][1]
            self.assertEqual(script.count('tokenEl.value.startsWith("v1.")'), 1)
            self.assertIn("tokenProvider: () => tokenEl.value || undefined", script)

    def test_no_secret_is_stored_in_the_browser(self) -> None:
        script = Collect()
        script.feed(PAGE.read_text(encoding="utf-8"))
        stored = re.findall(r"localStorage\.setItem\(\s*\"([^\"]+)\"", script.scripts[0][1])
        self.assertEqual(stored, ["brisvox.server"])
        demo = Collect()
        demo.feed(DEMO.read_text(encoding="utf-8"))
        self.assertNotIn("localStorage", demo.scripts[0][1])

    def test_basic_page_hygiene(self) -> None:
        for _, text, _ in self.each():
            self.assertTrue(text.lstrip().lower().startswith("<!doctype html>"))
            self.assertIn('<meta name="viewport"', text)
            self.assertIn("prefers-color-scheme: dark", text)
            self.assertRegex(text, r"<title>[^<]{3,}</title>")

    @unittest.skipUnless(shutil.which("node"), "node is not installed")
    def test_the_scripts_parse(self) -> None:
        for name, _, page in self.each():
            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "page.mjs"
                path.write_text(page.scripts[0][1], encoding="utf-8")
                done = subprocess.run(["node", "--check", str(path)], capture_output=True, text=True, timeout=30)
            self.assertEqual(done.returncode, 0, f"{name}: {done.stderr}")

    def test_the_shared_module_exists_and_exports_the_api(self) -> None:
        text = MODULE.read_text(encoding="utf-8")
        for needle in ("export class BrisvoxCall", "export async function connect", "export const WORKLET_SOURCE"):
            self.assertIn(needle, text)


if __name__ == "__main__":
    unittest.main()
