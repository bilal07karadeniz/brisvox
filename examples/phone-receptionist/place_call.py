"""Places an outbound reminder call through Twilio's REST API with answering-machine detection.

Standard library only. The callee answers; Twilio detects whether a person or a machine did (the
synchronous ``DetectMessageEnd`` mode: the webhook runs once the verdict is known and receives
``AnsweredBy``), then fetches TwiML from ``twiml_server.py``, which passes the verdict to
brisvox-server as the stream parameter ``answered_by``. With ``reminder.toml`` a machine is played
the voicemail message after its greeting and the call is hung up.

    export TWILIO_ACCOUNT_SID=AC...  TWILIO_AUTH_TOKEN=...  REMINDER_FROM=+1...
    export TWIML_PUBLIC_URL=https://YOUR-HOST/voice
    python place_call.py +15555550123          # the callee, E.164

Only call people who agreed to be called. Nothing is sent without ``--yes``.
"""

from __future__ import annotations

import argparse
import base64
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.twilio.com/2010-04-01"
E164 = re.compile(r"^\+[1-9][0-9]{7,14}$")

AMD = {
    "MachineDetection": "DetectMessageEnd",  # waits for the end of the greeting: a message can follow
    "MachineDetectionTimeout": "30",
}


def build_request(
    account_sid: str, auth_token: str, from_number: str, to_number: str, webhook_url: str
) -> urllib.request.Request:
    """The ``Calls`` request: ``To`` rings, ``Url`` is the voice webhook, detection is on."""
    for name, number in (("from", from_number), ("to", to_number)):
        if not E164.match(number):
            raise ValueError(f"the {name} number must be E.164 (+ and 8 to 15 digits)")
    if not webhook_url.startswith("https://"):
        raise ValueError("the webhook URL must be https://")
    if not re.match(r"^AC[0-9a-fA-F]{32}$", account_sid):
        raise ValueError("the account SID must look like AC followed by 32 hex digits")
    form = {"To": to_number, "From": from_number, "Url": webhook_url, "Method": "POST", **AMD}
    request = urllib.request.Request(
        f"{API}/Accounts/{account_sid}/Calls.json",
        data=urllib.parse.urlencode(form).encode(),
        method="POST",
    )
    basic = base64.b64encode(f"{account_sid}:{auth_token}".encode()).decode()
    request.add_header("Authorization", f"Basic {basic}")
    request.add_header("Content-Type", "application/x-www-form-urlencoded")
    return request


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("to", help="the callee, E.164")
    parser.add_argument("--yes", action="store_true", help="really place the call")
    args = parser.parse_args(argv)
    env = os.environ
    missing = [k for k in ("TWILIO_ACCOUNT_SID", "TWILIO_AUTH_TOKEN", "REMINDER_FROM", "TWIML_PUBLIC_URL") if not env.get(k)]
    if missing:
        sys.exit("missing environment: " + ", ".join(missing))
    try:
        request = build_request(
            env["TWILIO_ACCOUNT_SID"], env["TWILIO_AUTH_TOKEN"], env["REMINDER_FROM"], args.to, env["TWIML_PUBLIC_URL"]
        )
    except ValueError as exc:
        sys.exit(str(exc))
    if not args.yes:
        print(f"dry run: would call {args.to} from {env['REMINDER_FROM']} (add --yes)")
        return
    try:
        with urllib.request.urlopen(request, timeout=15) as response:  # noqa: S310 (https, fixed host)
            print(response.status, response.read().decode()[:300])
    except urllib.error.HTTPError as exc:
        sys.exit(f"Twilio answered {exc.code}: {exc.read().decode()[:300]}")


if __name__ == "__main__":
    main()
