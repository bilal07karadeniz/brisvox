"""The voice webhook of the phone receptionist: answers Twilio with the TwiML that connects the
call to brisvox-server's Twilio Media Streams listener.

Standard library only. Twilio POSTs here when a call arrives on your number (inbound) or when an
outbound call you placed with ``place_call.py`` is answered (the webhook of ``Url``)::

    export BRISVOX_STREAM_URL=wss://YOUR-HOST/twilio/stream     # = BRISVOX_TWILIO_PUBLIC_URL
    export BRISVOX_TWILIO_STREAM_SECRET=...                     # the same value as the server's
    export TWILIO_AUTH_TOKEN=...                                # verify X-Twilio-Signature
    export TWIML_PUBLIC_URL=https://YOUR-HOST/voice             # the URL Twilio is configured with
    python twiml_server.py --port 8000

It listens on ``127.0.0.1`` unless told otherwise. In a container (or any setup where the proxy
is not on the same host) pass ``--host 0.0.0.0`` so the proxy can reach it, and keep the port off
the public network: the proxy and the Twilio signature check are the front door.

What it does, and no more:

* ``POST /voice`` returns ``<Response><Connect><Stream url="..."/></Connect></Response>``. With
  ``BRISVOX_TWILIO_STREAM_SECRET`` set (recommended) the stream URL is the base URL plus a signed,
  single-use **per-call segment** minted from the call's ``CallSid``, so a captured URL cannot be
  replayed (``docs/providers/twilio.md``, "Per-call stream URLs"). Without the secret it answers
  with the fixed base URL, which the server accepts only with ``BRISVOX_TWILIO_ALLOW_STATIC_URL=1``
  (legacy: replayable). When
  Twilio's synchronous answering-machine detection ran (outbound call with ``MachineDetection``)
  the form carries ``AnsweredBy``; it is passed on as the stream parameter ``answered_by``, which
  the server reads at the call's start (``docs/TELEPHONY.md`` section 6, "Voicemail in the binary").
  The call's ``From`` and ``To`` go along as the stream parameters ``from`` and ``to``: the
  server hands them to an SDK agent only when the caller identity is opted in
  (``docs/AGENT_PROTOCOL.md``, "Caller identity"), so they are inert otherwise.
* With ``TWILIO_AUTH_TOKEN`` set, a request whose ``X-Twilio-Signature`` does not match is refused
  with 403 (a webhook anyone can call would let anyone start calls on your provider keys). Without
  a token the check is off and the server says so at start: only for a first local try.
* ``GET /healthz`` answers ``ok``.

The Twilio request validation is the documented algorithm: base64(HMAC-SHA1(auth token, full URL
followed by every POST parameter name and value, names sorted)).
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl
from xml.sax.saxutils import quoteattr

MAX_BODY = 16 * 1024
"""Largest webhook body read (a Twilio voice webhook is well under 4 KiB)."""

CALL_SID = re.compile(r"^[A-Za-z0-9]{1,64}$")
"""Twilio call SIDs are ``CA`` plus 32 hex digits; the server accepts ASCII letters and digits."""

STREAM_TOKEN_DOMAIN = "brisvox-twilio-stream-v1"
"""Domain-separation prefix of the segment's MAC (fixed by brisvox-server)."""

MIN_SECRET_BYTES = 32
"""Shortest ``BRISVOX_TWILIO_STREAM_SECRET`` the server accepts."""

DEFAULT_TTL = 120
"""Seconds a minted URL stays valid: the time Twilio needs to open the stream, not the call."""

ANSWERED_BY = re.compile(r"^[a-z_]{1,32}$")
"""Twilio ``AnsweredBy`` values are lower-case words with underscores (``machine_end_beep``)."""

CALL_PARTY = re.compile(r"^[0-9A-Za-z+*#._:@-]{1,64}$")
"""What is passed on of Twilio's ``From`` / ``To``: an E.164 number (``+15551230001``), or the
short identifiers Twilio uses for client and SIP legs. Anything else is dropped, never escaped."""


def twiml(
    stream_url: str,
    answered_by: str | None = None,
    caller: str | None = None,
    called: str | None = None,
) -> str:
    """The TwiML that connects the call to the Media Streams listener at ``stream_url``.

    ``caller`` and ``called`` (Twilio's ``From`` and ``To``) become the stream parameters
    ``from`` and ``to``; ``answered_by`` (Twilio's ``AnsweredBy``) becomes ``answered_by``.
    Anything that does not look like Twilio's values is dropped.

    The server forwards ``from`` and ``to`` to an SDK agent process only when the caller
    identity is opted in (``[agent_link] caller_identity = true`` or
    ``BRISVOX_AGENT_CALLER_IDENTITY=1``, ``docs/AGENT_PROTOCOL.md`` "Caller identity"), and
    withholds them while it redacts phone numbers; otherwise they are ignored. Stream parameters
    are not covered by the stream URL's signature: treat them as hints, like a caller ID.
    """
    if not stream_url.startswith("wss://"):
        raise ValueError("the stream URL must be a wss:// URL (Twilio refuses anything else)")
    parameter = ""
    for name, value in (("from", caller), ("to", called)):
        if value and CALL_PARTY.fullmatch(value):
            parameter += f'<Parameter name="{name}" value="{value}"/>'
    if answered_by and ANSWERED_BY.fullmatch(answered_by):
        parameter += f'<Parameter name="answered_by" value="{answered_by}"/>'
    stream = f"<Stream url={quoteattr(stream_url)}>{parameter}</Stream>"
    return f'<?xml version="1.0" encoding="UTF-8"?><Response><Connect>{stream}</Connect></Response>'


def per_call_stream_url(
    base_url: str, secret: str, call_sid: str, *, ttl: int = DEFAULT_TTL, now: int | None = None
) -> str:
    """``base_url`` plus the signed single-use segment ``<callSid>.<exp>.<mac>`` for one call.

    ``mac = base64url_nopad(HMAC-SHA256(secret, "brisvox-twilio-stream-v1|<callSid>|<exp>"))`` and
    ``exp`` is the Unix time (seconds) at which the URL dies. ``now`` is for tests.
    """
    if not CALL_SID.fullmatch(call_sid):
        raise ValueError("the call SID must be ASCII letters and digits")
    if len(secret.encode()) < MIN_SECRET_BYTES:
        raise ValueError(f"the stream secret must be at least {MIN_SECRET_BYTES} bytes")
    if not 1 <= ttl <= 3600:
        raise ValueError("ttl must be between 1 and 3600 seconds")
    exp = (int(time.time()) if now is None else now) + ttl
    message = f"{STREAM_TOKEN_DOMAIN}|{call_sid}|{exp}".encode()
    digest = hmac.new(secret.encode(), message, hashlib.sha256).digest()
    mac = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
    return f"{base_url.rstrip('/')}/{call_sid}.{exp}.{mac}"


def twilio_signature(auth_token: str, url: str, params: list[tuple[str, str]]) -> str:
    """The ``X-Twilio-Signature`` Twilio computes for a POST to ``url`` with ``params``."""
    signed = url + "".join(name + value for name, value in sorted(params))
    digest = hmac.new(auth_token.encode(), signed.encode(), hashlib.sha1).digest()
    return base64.b64encode(digest).decode()


def signature_ok(auth_token: str, url: str, params: list[tuple[str, str]], given: str | None) -> bool:
    """Is ``given`` the signature of this request? Compared in constant time."""
    if not given:
        return False
    return hmac.compare_digest(twilio_signature(auth_token, url, params), given)


class Webhook(BaseHTTPRequestHandler):
    """Serves ``POST /voice`` and ``GET /healthz``."""

    server_version = "twiml-server"
    timeout = 10
    """Socket timeout in seconds, for the request line, the headers and the body: a client that
    stalls (slowloris) loses its connection instead of pinning a server thread forever."""
    stream_url = ""
    auth_token: str | None = None
    public_url: str | None = None
    stream_secret: str | None = None
    quiet = False

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        if self.quiet:
            return
        sys.stderr.write("%s - %s\n" % (self.address_string(), format % args))

    def _reply(self, status: int, body: str, content_type: str = "text/plain; charset=utf-8") -> None:
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/healthz":
            self._reply(200, "ok")
        else:
            self._reply(404, "not found")

    def do_POST(self) -> None:  # noqa: N802
        if self.path.split("?", 1)[0] != "/voice":
            self._reply(404, "not found")
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = -1
        if not 0 <= length <= MAX_BODY:
            self._reply(413, "body too large")
            return
        params = parse_qsl(self.rfile.read(length).decode("utf-8", "replace"), keep_blank_values=True)
        if self.auth_token:
            url = self.public_url or ""
            if not signature_ok(self.auth_token, url, params, self.headers.get("X-Twilio-Signature")):
                self._reply(403, "bad signature")
                return
        form = dict(params)
        stream_url = self.stream_url
        if self.stream_secret:
            try:
                stream_url = per_call_stream_url(stream_url, self.stream_secret, form.get("CallSid", ""))
            except ValueError:
                self._reply(400, "missing or malformed CallSid")
                return
        body = twiml(stream_url, form.get("AnsweredBy"), form.get("From"), form.get("To"))
        self._reply(200, body, "text/xml; charset=utf-8")


def make_server(
    host: str,
    port: int,
    stream_url: str,
    *,
    auth_token: str | None,
    public_url: str | None,
    stream_secret: str | None = None,
    quiet: bool = False,
) -> ThreadingHTTPServer:
    """A webhook server bound to ``host:port`` (port 0: a free one)."""
    twiml(stream_url)  # fail at start, not on the first call
    if auth_token and not public_url:
        raise ValueError("TWIML_PUBLIC_URL is required with TWILIO_AUTH_TOKEN (the signed URL)")
    if stream_secret:
        per_call_stream_url(stream_url, stream_secret, "CA0")  # fail at start on a short secret
    handler = type(
        "BoundWebhook",
        (Webhook,),
        {
            "stream_url": stream_url,
            "auth_token": auth_token,
            "public_url": public_url,
            "stream_secret": stream_secret or None,
            "quiet": quiet,
        },
    )
    return ThreadingHTTPServer((host, port), handler)


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description="Twilio voice webhook for brisvox-server")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    args = parser.parse_args(argv)
    stream_url = os.environ.get("BRISVOX_STREAM_URL", "")
    if not stream_url:
        sys.exit("BRISVOX_STREAM_URL is required (wss://YOUR-HOST/twilio/stream)")
    token = os.environ.get("TWILIO_AUTH_TOKEN") or None
    public_url = os.environ.get("TWIML_PUBLIC_URL") or None
    secret = os.environ.get("BRISVOX_TWILIO_STREAM_SECRET") or None
    if token is None:
        print("WARNING: TWILIO_AUTH_TOKEN is not set: requests are NOT verified", file=sys.stderr)
    if secret is None:
        print(
            "WARNING: BRISVOX_TWILIO_STREAM_SECRET is not set: answering with the fixed stream URL "
            "(legacy; the server needs BRISVOX_TWILIO_ALLOW_STATIC_URL=1 and a captured signature can be replayed)",
            file=sys.stderr,
        )
    try:
        server = make_server(
            args.host, args.port, stream_url, auth_token=token, public_url=public_url, stream_secret=secret
        )
    except ValueError as exc:
        sys.exit(str(exc))
    print(f"LISTENING http://{args.host}:{server.server_address[1]}/voice", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
