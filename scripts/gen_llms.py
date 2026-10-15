#!/usr/bin/env python3
"""Generate llms.txt and llms-full.txt (https://llmstxt.org) for AI coding agents.

llms.txt is the map: what brisvox is, the facts an agent needs before it writes anything, and one
line per document saying when to read it. llms-full.txt is the user guide in one file: the
documents an agent needs to build, configure and deploy an agent (not the design notes, which
are in llms.txt's "Optional" section).

    python3 scripts/gen_llms.py           # rewrite both files
    python3 scripts/gen_llms.py --check   # exit 1 if either file is stale or a listed file is gone

Standard library only. verify.sh runs --check.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPO_URL = "https://github.com/bilal07karadeniz/brisvox"

HEADER = """\
# brisvox

> Rust-native, open-source engine for real-time voice agents (phone calls and browser sessions):
> the caller talks, the agent listens (STT), thinks (an LLM with tools) and answers (TTS), and the
> caller can interrupt at any moment. One `brisvox-server` process serves hundreds of concurrent
> calls with one agent definition (a TOML or YAML file); your own tools run in your application
> through the Python or TypeScript SDK, or in Rust.

Paths below are relative to the repository root ({repo}).

Things to know before writing anything:

- An agent is a file (`BRISVOX_AGENT_FILE`): `[agent]` (name, language, system_prompt),
  `[stt]`, `[llm]`, `[tts]` (each `provider = "..."` plus a `[<kind>.<provider>]` table),
  optional `[turn]`, `[interruption]`, `[tools]`, `[s2s]`. Start from a file in
  `examples/agents/` (every one is loaded by the test suite) rather than from memory.
- The server itself is configured only by environment variables (`BRISVOX_*`, listed in
  docs/OPERATIONS.md); API keys come from the environment variable a provider table names
  (`api_key_env`), never from the agent file.
- Providers are Cargo features: build with `--features all-providers` (or the one you need, e.g.
  `provider-deepgram`); a definition naming a provider that is not compiled in is refused at
  startup with the feature to turn on. Transports are features too: `webrtc`, `sip`.
- Tools: declare them in your application with the Python SDK (`sdk/python`, `pip install
  brisvox`, module `brisvox`; the package `brisvox-agent`, module `brisvox_agent`, is the same
  SDK) or the TypeScript SDK (`sdk/typescript`, `npm i brisvox`; `@brisvox/agent` is the same
  SDK, and `brisvox/web` or `@brisvox/web` the browser module); the server
  connects to your agent over the agent protocol (`BRISVOX_AGENT_URL`). Mark tools that change
  the world as side effects: they never run speculatively and never run twice.
- Listeners fail closed: off loopback the server refuses to start without authentication
  (`BRISVOX_SESSION_SECRET` or a static token). For browsers use the WebRTC listener; it falls
  back to WebSocket audio on the same port when WebRTC cannot connect (one-port hosts such as
  Railway). On a website, use the browser module in `sdk/web` instead of writing audio code.
- Validate before running: `brisvox-server check <agent file>` runs the real loader offline and
  reports every problem with its key path and line (missing keys are warnings unless
  `--require-secrets`).
- The server does not read a `.env` file: export the variables (or use your platform's
  environment settings).
- Tools written in Rust run inside your own binary: a three-line `main` over
  `brisvox::run_with_tools` (`cargo add brisvox`; the same function as
  `brisvox_server::app::run_with_tools`; see examples/rust-tools).
- Which Cargo features a goal needs (browser, phone, SIP, local speech, Rust tools) is the
  "What to build with" table in README.md.
"""

# (section, path, description). Section "Optional" is the llmstxt.org convention for material an
# agent can skip. `full` marks the documents copied into llms-full.txt.
DOCS: list[tuple[str, str, str, bool]] = [
    ("Start here", "skills/brisvox/SKILL.md", "The workflow and the common traps in two pages (an Agent Skill)", True),
    ("Start here", "book/src/introduction.md", "What brisvox is and how the parts fit", True),
    ("Start here", "book/src/quickstart/server.md", "Build the server, run an example agent, call it", True),
    ("Start here", "book/src/quickstart/sdk-agent.md", "Write an agent with your own tools in Python or TypeScript", True),
    ("Start here", "book/src/quickstart/docker.md", "Run with Docker and compose", True),
    ("Start here", "book/src/quickstart/local.md", "A fully local assistant: sherpa-onnx, Ollama, Kokoro, nothing leaves the machine", True),
    ("Start here", "examples/README.md", "The runnable examples: web assistant, phone receptionist, booking agent", True),
    ("Configuration", "docs/CONFIG.md", "Every agent-file key: agent, stt, llm, tts, turn, interruption, tools, s2s, TOML and YAML", True),
    ("Configuration", "docs/OPERATIONS.md", "Every server environment variable, listeners, health, metrics, usage and cost (LLM tokens, STT seconds, TTS characters), overload and admission, limits, shutdown", True),
    ("Configuration", "docs/PROVIDERS.md", "Which STT/LLM/TTS/S2S providers exist, their config names and Cargo features", True),
    ("Tools", "book/src/concepts/tools.md", "Tool calling in short: read-only vs side-effect, fillers, barge-in", True),
    ("Tools", "docs/TOOLS.md", "The tool-calling contract: parallel and chained calls, retries, idempotency, confirmation", True),
    ("Tools", "sdk/python/README.md", "Python SDK: declare tools, run the agent server, events", True),
    ("Tools", "sdk/typescript/README.md", "TypeScript SDK: declare tools, run the agent server, events", True),
    ("Tools", "examples/rust-tools/README.md", "Rust tools inside your own binary (brisvox_server::app::run_with_tools)", True),
    ("Tools", "docs/AGENT_PROTOCOL.md", "The wire protocol between the server and an SDK agent: events incl. per-turn metrics and the per-call usage summary (session.usage)", False),
    ("Connecting callers", "sdk/web/README.md", "Browser client module for websites: WebRTC with automatic WebSocket fallback, Next.js example", True),
    ("Connecting callers", "docs/WEBSOCKET_PROTOCOL.md", "Generic websocket audio protocol (your own client or the browser fallback)", True),
    ("Connecting callers", "docs/providers/webrtc.md", "Browser calls over WebRTC: WHIP signaling, ICE, TURN, the WebSocket fallback", False),
    ("Connecting callers", "docs/RTVI.md", "Connect Pipecat's RTVI client SDKs to brisvox", True),
    ("Connecting callers", "docs/TELEPHONY.md", "Phone calls: DTMF, transfer, answering machines; Twilio, Telnyx, SIP", True),
    ("Connecting callers", "docs/providers/twilio.md", "Twilio Media Streams details and setup", False),
    ("Connecting callers", "docs/providers/telnyx.md", "Telnyx media streaming details and setup", False),
    ("Connecting callers", "docs/providers/sip.md", "SIP trunk listener (UDP/TCP/TLS, SRTP)", False),
    ("Deploying", "docs/DEPLOY.md", "Containers, one-port HTTPS hosts without UDP (Railway as an example), TURN, TLS, scaling", True),
    ("Deploying", "docs/SECURITY.md", "Threat model and the secure deployment checklist (tokens, TLS, limits)", True),
    ("Deploying", "docs/PRIVACY.md", "Recording (transcripts with a per-call summary line), encryption at rest, retention, redaction", True),
    ("Providers", "docs/providers/deepgram.md", "Deepgram STT", False),
    ("Providers", "docs/providers/assemblyai.md", "AssemblyAI STT", False),
    ("Providers", "docs/providers/gladia.md", "Gladia STT", False),
    ("Providers", "docs/providers/sherpa-onnx.md", "sherpa-onnx local streaming STT (self-hosted)", False),
    ("Providers", "docs/providers/openai-realtime-stt.md", "OpenAI Realtime transcription and local dialects (Speaches, vLLM, NVIDIA NIM)", False),
    ("Providers", "docs/providers/openai.md", "OpenAI LLM", False),
    ("Providers", "docs/providers/openai-compatible.md", "OpenAI-compatible LLMs: OpenRouter, Groq, Together, Fireworks, vLLM, Ollama", False),
    ("Providers", "docs/providers/anthropic.md", "Anthropic Claude LLM", False),
    ("Providers", "docs/providers/gemini.md", "Google Gemini LLM", False),
    ("Providers", "docs/providers/cartesia.md", "Cartesia TTS", False),
    ("Providers", "docs/providers/elevenlabs.md", "ElevenLabs TTS", False),
    ("Providers", "docs/providers/deepgram-aura.md", "Deepgram Aura TTS", False),
    ("Providers", "docs/providers/openai-speech.md", "Local TTS servers over /v1/audio/speech: Kokoro, Chatterbox, Speaches, vLLM-Omni", False),
    ("Providers", "docs/providers/local-speech.md", "Survey of local STT/TTS servers: latency, Turkish, licenses", False),
    ("Providers", "docs/S2S.md", "Speech-to-speech models (OpenAI Realtime, Gemini Live) as one stage", False),
    ("Providers", "docs/providers/openai-realtime.md", "OpenAI Realtime speech-to-speech", False),
    ("Providers", "docs/providers/gemini-live.md", "Gemini Live speech-to-speech", False),
    ("Providers", "docs/providers/call-control.md", "Telephony call control on Twilio and Telnyx", False),
    ("Optional", "docs/TURKISH.md", "Turkish quality: normalisation, numbers, providers that handle Turkish well", False),
    ("Optional", "docs/ARCHITECTURE.md", "Architecture and design decisions (crates, frames, epochs, pools)", False),
    ("Optional", "docs/PIPELINE.md", "Production pipeline design in depth", False),
    ("Optional", "docs/LATENCY.md", "Latency budget, measurements and capacity benchmarks", False),
    ("Optional", "docs/TESTING.md", "Testing strategy, simulations, live tests", False),
    ("Optional", "docs/TRACING.md", "Error and tracing conventions for contributors", False),
    ("Optional", "docs/RISKS.md", "Risk register", False),
    ("Optional", "book/src/benchmarks.md", "Benchmark results against Pipecat", False),
]

SECTION_ORDER = ["Start here", "Configuration", "Tools", "Connecting callers", "Deploying", "Providers", "Optional"]


def example_agents() -> list[Path]:
    return sorted((ROOT / "examples" / "agents").rglob("*.toml"))


def build_index() -> str:
    out = [HEADER.format(repo=REPO_URL)]
    for section in SECTION_ORDER:
        out.append(f"\n## {section}\n\n")
        for sec, path, desc, _ in DOCS:
            if sec == section:
                out.append(f"- [{title_of(path)}]({path}): {desc}\n")
        if section == "Start here":
            for p in example_agents():
                rel = p.relative_to(ROOT).as_posix()
                out.append(f"- [{p.name}]({rel}): example agent file ({first_comment(p)})\n")
    return "".join(out)


def build_full() -> str:
    out = [
        "# brisvox: the user guide in one file\n\n",
        "> Generated by scripts/gen_llms.py from the documents marked below; the index is llms.txt.\n",
        "> Each document starts with a `<!-- source: path -->` line.\n",
    ]
    for _, path, _, full in DOCS:
        if full:
            out.append(f"\n\n<!-- source: {path} -->\n\n")
            out.append(strip_ids(read(ROOT / path)).strip() + "\n")
    for p in example_agents():
        rel = p.relative_to(ROOT).as_posix()
        out.append(f"\n\n<!-- source: {rel} -->\n\n```toml\n{read(p).strip()}\n```\n")
    return "".join(out)


def read(p: Path) -> str:
    return p.read_text(encoding="utf-8").replace("\r\n", "\n")


# Internal task and decision ids (a "T-" or "D-" prefix and a number) in the docs mean nothing
# to a reader of llms*.txt. The generated files drop such id tokens with their
# separators and keep the rest of a parenthetical; ids used as words in a sentence stay.
_ID = r"[TD]-\d+(?:\.\d+)*[a-z]?(?:-live)?"
_ONLY_IDS = re.compile(r"\s*\(\s*" + _ID + r"(?:\s*[,;/]\s*" + _ID + r")*\s*\)")
_ID_AFTER_SEP = re.compile(r"\s*[,;]\s*" + _ID + r"\b(?=[^()\n]*\))")
_ID_FIRST = re.compile(r"(?<=\()\s*" + _ID + r"\s*[,;:]\s*")
_DASH_ID = re.compile(r"\s+[—-]\s+" + _ID + r"\b")


def strip_ids(text: str) -> str:
    for pattern in (_ONLY_IDS, _ID_AFTER_SEP, _ID_FIRST, _DASH_ID):
        text = pattern.sub("", text)
    return text


def title_of(path: str) -> str:
    for line in read(ROOT / path).splitlines():
        if line.startswith("# "):
            return strip_ids(line[2:]).strip()
    return path


def first_comment(p: Path) -> str:
    """The file's leading comment up to its first sentence end, at most ~160 characters, never
    ending inside a parenthesis."""
    words: list[str] = []
    for line in read(p).splitlines():
        s = line.strip()
        if not s.startswith("#"):
            break
        words.extend(s.lstrip("#").split())
    text = strip_ids(" ".join(words))
    end = text.find(". ")
    if 0 < end < 160:
        return text[:end]
    if len(text) <= 160:
        return text.rstrip(".")
    cut = text[:157].rsplit(" ", 1)[0]
    if cut.count("(") > cut.count(")"):
        cut = cut[: cut.rfind("(")].rstrip(" ,;:")
    return cut + "..."


def main(argv: list[str]) -> int:
    missing = [path for _, path, _, _ in DOCS if not (ROOT / path).is_file()]
    if missing:
        print("gen_llms: listed files are missing: " + ", ".join(missing), file=sys.stderr)
        return 1
    files = {ROOT / "llms.txt": build_index(), ROOT / "llms-full.txt": build_full()}
    if "--check" in argv:
        stale = [p.name for p, text in files.items() if not p.is_file() or read(p) != text]
        if stale:
            print(f"gen_llms: stale {', '.join(stale)}; run python3 scripts/gen_llms.py", file=sys.stderr)
            return 1
        print("gen_llms: ok")
        return 0
    for p, text in files.items():
        p.write_text(text, encoding="utf-8", newline="\n")
        print(f"gen_llms: wrote {p.name} ({len(text)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
