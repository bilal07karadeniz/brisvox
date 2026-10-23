#!/usr/bin/env python3
"""Runs sherpa-onnx's own streaming_server.py with one change: the recognized text is sent in
sentence case instead of the model's capital letters.

The English zipformer writes UPPERCASE text without punctuation. A small language model that is
given a conversation in capitals answers in capitals (measured: a 1.5B model did in 3 of 14
replies, a 1B model in all of them), and a text-to-speech voice spells capitalized words out.
Lowercase text with a capital first letter and a capital "I" reads like ordinary speech
transcripts. Nothing else is changed: audio, decoding, endpointing and the protocol are the
upstream script's, which is imported unmodified (pinned by SHA-256 in the Dockerfile).
"""

import json as _json
import re

import streaming_server as upstream

_PRONOUN_I = re.compile(r"\bi\b")


def sentence_case(text: str) -> str:
    """`IT'S I THINK` -> `It's I think`. Lowercase, a capital "I", a capital first letter."""
    text = _PRONOUN_I.sub("I", text.lower())
    return text[:1].upper() + text[1:]


class _Json:
    """Stands in for the `json` module inside streaming_server.py, which only calls `dumps`."""

    @staticmethod
    def dumps(obj, *args, **kwargs):
        if isinstance(obj, dict) and isinstance(obj.get("text"), str):
            obj = {**obj, "text": sentence_case(obj["text"])}
        return _json.dumps(obj, *args, **kwargs)


upstream.json = _Json

if __name__ == "__main__":
    upstream.setup_logger("log/log-streaming-server")
    upstream.main()
