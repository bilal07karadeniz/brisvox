"""Tests of the phone receptionist's helpers (no keys, no network beyond loopback):

    python3 -m unittest discover -s examples/phone-receptionist -p 'test_*.py'
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import io
import socket
import threading
import time
import unittest
import unittest.mock
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from contextlib import redirect_stdout

import place_call
import twiml_server

STREAM = "wss://agent.example.com/twilio/stream"
SID = "AC" + "0" * 32


class Twiml(unittest.TestCase):
    def test_connects_the_call_to_the_stream(self) -> None:
        root = ET.fromstring(twiml_server.twiml(STREAM))
        self.assertEqual(root.tag, "Response")
        stream = root.find("./Connect/Stream")
        assert stream is not None
        self.assertEqual(stream.get("url"), STREAM)
        self.assertEqual(list(stream), [])

    def test_the_machine_verdict_becomes_a_stream_parameter(self) -> None:
        root = ET.fromstring(twiml_server.twiml(STREAM, "machine_end_beep"))
        parameter = root.find("./Connect/Stream/Parameter")
        assert parameter is not None
        self.assertEqual(parameter.attrib, {"name": "answered_by", "value": "machine_end_beep"})

    def test_a_verdict_that_is_not_twilios_shape_is_dropped(self) -> None:
        for hostile in ('x"/><Say>hi</Say><Parameter a="', "", "Machine", "a" * 40):
            with self.subTest(hostile=hostile):
                root = ET.fromstring(twiml_server.twiml(STREAM, hostile))
                self.assertIsNone(root.find("./Connect/Stream/Parameter"))
                self.assertIsNone(root.find(".//Say"))

    def test_from_and_to_become_stream_parameters(self) -> None:
        root = ET.fromstring(twiml_server.twiml(STREAM, "human", "+15551230001", "+15551230002"))
        parameters = [p.attrib for p in root.findall("./Connect/Stream/Parameter")]
        self.assertEqual(
            parameters,
            [
                {"name": "from", "value": "+15551230001"},
                {"name": "to", "value": "+15551230002"},
                {"name": "answered_by", "value": "human"},
            ],
        )

    def test_a_party_that_is_not_a_number_or_short_id_is_dropped(self) -> None:
        hostile = 'x"/><Say>hi</Say><Parameter a="'
        for bad in (hostile, "", "+" + "1" * 80, "a b", "<Say/>", "+1555\n"):
            with self.subTest(bad=bad):
                root = ET.fromstring(twiml_server.twiml(STREAM, None, bad, bad))
                self.assertIsNone(root.find("./Connect/Stream/Parameter"))
                self.assertIsNone(root.find(".//Say"))
        # Twilio's client and SIP legs use short identifiers.
        root = ET.fromstring(twiml_server.twiml(STREAM, None, "client:alice", "sip:desk@pbx.example"))
        values = [p.get("value") for p in root.findall("./Connect/Stream/Parameter")]
        self.assertEqual(values, ["client:alice", "sip:desk@pbx.example"])

    def test_the_url_is_escaped_and_must_be_wss(self) -> None:
        root = ET.fromstring(twiml_server.twiml("wss://h.example/p?a=1&b=2"))
        stream = root.find("./Connect/Stream")
        assert stream is not None
        self.assertEqual(stream.get("url"), "wss://h.example/p?a=1&b=2")
        for bad in ("ws://h.example/p", "https://h.example/p", ""):
            with self.subTest(url=bad), self.assertRaises(ValueError):
                twiml_server.twiml(bad)


class Signature(unittest.TestCase):
    PARAMS = [("To", "+18005551212"), ("CallSid", "CA1"), ("From", "+12349013030")]
    URL = "https://agent.example.com/voice"

    def test_follows_the_documented_algorithm(self) -> None:
        # URL + parameter names and values, names sorted, HMAC-SHA1 with the Auth Token, base64.
        signed = self.URL + "CallSidCA1From+12349013030To+18005551212"
        expected = base64.b64encode(hmac.new(b"token", signed.encode(), hashlib.sha1).digest()).decode()
        self.assertEqual(twiml_server.twilio_signature("token", self.URL, self.PARAMS), expected)

    def test_accepts_only_the_right_signature(self) -> None:
        good = twiml_server.twilio_signature("token", self.URL, self.PARAMS)
        self.assertTrue(twiml_server.signature_ok("token", self.URL, self.PARAMS, good))
        self.assertFalse(twiml_server.signature_ok("other", self.URL, self.PARAMS, good))
        self.assertFalse(twiml_server.signature_ok("token", self.URL + "x", self.PARAMS, good))
        self.assertFalse(twiml_server.signature_ok("token", self.URL, self.PARAMS + [("X", "1")], good))
        self.assertFalse(twiml_server.signature_ok("token", self.URL, self.PARAMS, None))
        self.assertFalse(twiml_server.signature_ok("token", self.URL, self.PARAMS, ""))


class WebhookServer(unittest.TestCase):
    def serve(self, **kwargs: str | None) -> str:
        server = twiml_server.make_server("127.0.0.1", 0, STREAM, quiet=True, **kwargs)  # type: ignore[arg-type]
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 5)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_address[1]}"

    @staticmethod
    def post(url: str, form: dict[str, str], signature: str | None = None) -> tuple[int, str]:
        request = urllib.request.Request(url, data=urllib.parse.urlencode(form).encode(), method="POST")
        if signature:
            request.add_header("X-Twilio-Signature", signature)
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                return response.status, response.read().decode()
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode()

    def test_unverified_mode_serves_twiml(self) -> None:
        base = self.serve(auth_token=None, public_url=None)
        status, body = self.post(base + "/voice", {"CallSid": "CA1", "AnsweredBy": "human"})
        self.assertEqual(status, 200)
        stream = ET.fromstring(body).find("./Connect/Stream")
        assert stream is not None
        self.assertEqual(stream.get("url"), STREAM)
        self.assertEqual(stream.find("Parameter").get("value"), "human")  # type: ignore[union-attr]
        with urllib.request.urlopen(base + "/healthz", timeout=5) as response:
            self.assertEqual(response.read(), b"ok")
        self.assertEqual(self.post(base + "/other", {})[0], 404)

    def test_the_webhooks_from_and_to_reach_the_stream(self) -> None:
        base = self.serve(auth_token=None, public_url=None)
        form = {"CallSid": "CA1", "From": "+15551230001", "To": "+15551230002"}
        status, body = self.post(base + "/voice", form)
        self.assertEqual(status, 200)
        parameters = {p.get("name"): p.get("value") for p in ET.fromstring(body).findall("./Connect/Stream/Parameter")}
        self.assertEqual(parameters, {"from": "+15551230001", "to": "+15551230002"})

    def test_a_client_that_stalls_loses_its_connection(self) -> None:
        # A short deadline for the test; the shipped one is a finite class attribute too.
        self.assertIsNotNone(twiml_server.Webhook.timeout)
        patcher = unittest.mock.patch.object(twiml_server.Webhook, "timeout", 0.3)
        patcher.start()
        self.addCleanup(patcher.stop)
        base = self.serve(auth_token=None, public_url=None)
        port = int(base.rsplit(":", 1)[1])
        stalls = (
            b"POST /voice HTTP/1.1\r\nHost: x\r\nContent-Length: 16000\r\n\r\n",  # promises a body
            b"POST /voice HTTP/1.1\r\nHost: x\r\n",  # headers never finish
            b"",  # connects and says nothing
        )
        for sent in stalls:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as conn:
                if sent:
                    conn.sendall(sent)
                try:
                    data = conn.recv(4096)  # the server closes (b"") instead of waiting forever
                except TimeoutError:
                    self.fail("the server kept a stalled connection open")
                self.assertNotIn(b"200", data)

    def test_verified_mode_refuses_unsigned_and_forged_requests(self) -> None:
        public = "https://agent.example.com/voice"
        base = self.serve(auth_token="secret", public_url=public)
        form = {"CallSid": "CA1", "From": "+12349013030"}
        self.assertEqual(self.post(base + "/voice", form)[0], 403)
        self.assertEqual(self.post(base + "/voice", form, "AAAA")[0], 403)
        signature = twiml_server.twilio_signature("secret", public, list(form.items()))
        status, body = self.post(base + "/voice", form, signature)
        self.assertEqual(status, 200)
        self.assertIn("<Stream", body)
        # The signature covers the parameters: adding one invalidates it.
        self.assertEqual(self.post(base + "/voice", {**form, "AnsweredBy": "machine_start"}, signature)[0], 403)

    def test_a_token_needs_the_public_url(self) -> None:
        with self.assertRaises(ValueError):
            twiml_server.make_server("127.0.0.1", 0, STREAM, auth_token="secret", public_url=None)


class PerCallStreamUrl(unittest.TestCase):
    """The segment brisvox-server verifies: ``<callSid>.<exp>.<mac>`` after the base URL."""

    SECRET = "0123456789abcdef0123456789abcdef-secret"

    def test_matches_the_servers_known_answer(self) -> None:
        # The same vector the transport crate pins (docs/providers/twilio.md).
        url = twiml_server.per_call_stream_url(STREAM, self.SECRET, "CAabc123", ttl=100, now=1_899_999_900)
        self.assertEqual(
            url, STREAM + "/CAabc123.1900000000.ixBbTZftQyn5JYCeFPFULx9O_UM1uxv0OZ_JhN6GIm0"
        )

    def test_a_trailing_slash_on_the_base_is_not_doubled(self) -> None:
        url = twiml_server.per_call_stream_url(STREAM + "/", self.SECRET, "CA1", now=1)
        self.assertTrue(url.startswith(STREAM + "/CA1.121."), url)

    def test_refuses_what_the_server_would_refuse(self) -> None:
        for call_sid in ("", "CA 1", "CA/1", "CA.1", "x" * 65):
            with self.subTest(call_sid=call_sid), self.assertRaises(ValueError):
                twiml_server.per_call_stream_url(STREAM, self.SECRET, call_sid)
        with self.assertRaises(ValueError):
            twiml_server.per_call_stream_url(STREAM, "too-short", "CA1")
        for ttl in (0, 3601):
            with self.subTest(ttl=ttl), self.assertRaises(ValueError):
                twiml_server.per_call_stream_url(STREAM, self.SECRET, "CA1", ttl=ttl)

    def test_uses_the_current_time_by_default(self) -> None:
        before = int(time.time())
        url = twiml_server.per_call_stream_url(STREAM, self.SECRET, "CA1", ttl=60)
        exp = int(url.rsplit("/", 1)[1].split(".")[1])
        self.assertTrue(before + 60 <= exp <= int(time.time()) + 60)


class PerCallWebhook(WebhookServer):
    """With the secret set, every call gets its own signed stream URL."""

    SECRET = PerCallStreamUrl.SECRET

    def test_each_call_gets_its_own_segment(self) -> None:
        base = self.serve(auth_token=None, public_url=None, stream_secret=self.SECRET)
        urls = []
        for sid in ("CA11111111111111111111111111111111", "CA22222222222222222222222222222222"):
            status, body = self.post(base + "/voice", {"CallSid": sid, "AnsweredBy": "human"})
            self.assertEqual(status, 200)
            stream = ET.fromstring(body).find("./Connect/Stream")
            assert stream is not None
            url = stream.get("url") or ""
            self.assertTrue(url.startswith(STREAM + "/" + sid + "."), url)
            self.assertEqual(stream.find("Parameter").get("value"), "human")  # type: ignore[union-attr]
            urls.append(url)
        self.assertNotEqual(urls[0], urls[1])

    def test_the_mac_verifies_with_the_secret(self) -> None:
        base = self.serve(auth_token=None, public_url=None, stream_secret=self.SECRET)
        sid = "CA11111111111111111111111111111111"
        _, body = self.post(base + "/voice", {"CallSid": sid})
        stream = ET.fromstring(body).find("./Connect/Stream")
        assert stream is not None
        segment = (stream.get("url") or "").rsplit("/", 1)[1]
        call_sid, exp, mac = segment.split(".")
        self.assertEqual(call_sid, sid)
        expected = hmac.new(self.SECRET.encode(), f"brisvox-twilio-stream-v1|{sid}|{exp}".encode(), hashlib.sha256)
        self.assertEqual(mac, base64.urlsafe_b64encode(expected.digest()).rstrip(b"=").decode())
        self.assertGreater(int(exp), int(time.time()))

    def test_a_request_without_a_usable_call_sid_is_refused(self) -> None:
        base = self.serve(auth_token=None, public_url=None, stream_secret=self.SECRET)
        for form in ({}, {"CallSid": ""}, {"CallSid": "CA/../x"}):
            with self.subTest(form=form):
                self.assertEqual(self.post(base + "/voice", form)[0], 400)

    def test_the_signature_check_still_applies(self) -> None:
        public = "https://agent.example.com/voice"
        base = self.serve(auth_token="secret", public_url=public, stream_secret=self.SECRET)
        form = {"CallSid": "CA11111111111111111111111111111111"}
        self.assertEqual(self.post(base + "/voice", form)[0], 403)
        signature = twiml_server.twilio_signature("secret", public, list(form.items()))
        self.assertEqual(self.post(base + "/voice", form, signature)[0], 200)

    def test_a_short_secret_fails_at_start(self) -> None:
        with self.assertRaises(ValueError):
            twiml_server.make_server("127.0.0.1", 0, STREAM, auth_token=None, public_url=None, stream_secret="short")


class PlaceCall(unittest.TestCase):
    def request(self, **overrides: str) -> urllib.request.Request:
        args = {
            "account_sid": SID,
            "auth_token": "token",
            "from_number": "+15555550100",
            "to_number": "+15555550123",
            "webhook_url": "https://agent.example.com/voice",
            **overrides,
        }
        return place_call.build_request(**args)

    def test_requests_machine_detection_with_the_message_end(self) -> None:
        request = self.request()
        self.assertEqual(request.full_url, f"https://api.twilio.com/2010-04-01/Accounts/{SID}/Calls.json")
        self.assertEqual(request.get_method(), "POST")
        form = dict(urllib.parse.parse_qsl((request.data or b"").decode()))  # type: ignore[arg-type]
        self.assertEqual(form["To"], "+15555550123")
        self.assertEqual(form["From"], "+15555550100")
        self.assertEqual(form["Url"], "https://agent.example.com/voice")
        self.assertEqual(form["MachineDetection"], "DetectMessageEnd")
        basic = base64.b64decode(request.get_header("Authorization").removeprefix("Basic "))
        self.assertEqual(basic.decode(), f"{SID}:token")

    def test_refuses_bad_input_before_any_request(self) -> None:
        for bad in (
            {"to_number": "5555550123"},
            {"from_number": "+1"},
            {"webhook_url": "http://agent.example.com/voice"},
            {"account_sid": "ACxyz"},
        ):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.request(**bad)

    def test_without_yes_nothing_is_sent(self) -> None:
        env = {
            "TWILIO_ACCOUNT_SID": SID,
            "TWILIO_AUTH_TOKEN": "token",
            "REMINDER_FROM": "+15555550100",
            "TWIML_PUBLIC_URL": "https://agent.example.com/voice",
        }
        out = io.StringIO()
        with unittest.mock.patch.dict("os.environ", env), redirect_stdout(out):
            with unittest.mock.patch("urllib.request.urlopen") as urlopen:
                place_call.main(["+15555550123"])
        urlopen.assert_not_called()
        self.assertIn("dry run", out.getvalue())


if __name__ == "__main__":
    unittest.main()
