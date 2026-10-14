"""The brisvox protocol adapter: handshake validation and the frame serializer."""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from pipecat.frames.frames import (  # noqa: E402
    CancelFrame,
    CancelWorkerFrame,
    EndFrame,
    InputAudioRawFrame,
    InputDTMFFrame,
    InterruptionFrame,
    OutputAudioRawFrame,
    OutputTransportMessageUrgentFrame,
)
from pipecat.processors.frame_processor import FrameProcessorSetup  # noqa: E402

from brisvox_protocol import (  # noqa: E402
    PLAYOUT_GRACE_SECS,
    BrisvoxFrameSerializer,
    ProtocolError,
    Start,
    parse_start,
    started_message,
)


class FakeClock:
    """A settable monotonic clock (seconds)."""

    def __init__(self):
        self.now = 0.0

    def __call__(self) -> float:
        return self.now


def start(**fields) -> str:
    message = {"type": "start", "version": 1, "sample_rate": 16000}
    message.update(fields)
    return json.dumps(message)


class ParseStart(unittest.TestCase):
    def test_a_minimal_start_takes_the_defaults(self):
        s = parse_start(start(), fallback_call_id="gen")
        self.assertEqual(s, Start(16000, "pcm16", "gen", {}))

    def test_fields_are_kept(self):
        s = parse_start(
            start(sample_rate=8000, encoding="mulaw", call_id="c1", metadata={"k": "v"}), "gen"
        )
        self.assertEqual(s, Start(8000, "mulaw", "c1", {"k": "v"}))

    def test_started_echoes_the_call(self):
        s = Start(24000, "pcm16", "abc", {})
        self.assertEqual(
            json.loads(started_message(s)),
            {
                "type": "started",
                "version": 1,
                "call_id": "abc",
                "sample_rate": 24000,
                "encoding": "pcm16",
            },
        )

    def test_faults_carry_the_brisvox_codes(self):
        cases = [
            (b"\x00\x01", "audio_before_start", 1008),
            ("not json", "invalid_json", 1002),
            ("[1]", "invalid_json", 1002),
            (json.dumps({"type": "dtmf", "digit": "1"}), "invalid_message", 1008),
            (start(version=2), "unsupported_version", 1008),
            (start(sample_rate=12345), "unsupported_sample_rate", 1008),
            (start(sample_rate=True), "unsupported_sample_rate", 1008),
            (start(encoding="opus"), "invalid_message", 1008),
            (start(call_id="x" * 129), "invalid_message", 1008),
            (start(metadata={"k": 1}), "invalid_message", 1008),
            ("x" * (16 * 1024 + 1), "frame_too_large", 1009),
        ]
        for data, code, close in cases:
            with self.subTest(code=code, data=str(data)[:40]):
                with self.assertRaises(ProtocolError) as caught:
                    parse_start(data, "gen")
                self.assertEqual(caught.exception.code, code)
                self.assertEqual(caught.exception.close_code, close)
                error = json.loads(caught.exception.to_json())
                self.assertEqual(error["type"], "error")
                self.assertTrue(error["fatal"])


def tone(samples: int, amplitude: int = 8000) -> bytes:
    return b"".join(((amplitude if n % 2 else -amplitude)).to_bytes(2, "little", signed=True) for n in range(samples))


class Serializer(unittest.IsolatedAsyncioTestCase):
    async def serializer(self, rate=16000, encoding="pcm16", in_rate=16000, clock=None):
        kwargs = {} if clock is None else {"clock": clock}
        s = BrisvoxFrameSerializer(Start(rate, encoding, "c", {}), **kwargs)
        setup = FrameProcessorSetup.__new__(FrameProcessorSetup)
        setup.audio_in_sample_rate = in_rate  # the only field the serializer reads
        await s.setup(setup)
        return s

    async def test_an_interruption_of_playing_audio_is_a_clear_with_a_new_epoch(self):
        clock = FakeClock()
        s = await self.serializer(clock=clock)
        await s.serialize(OutputAudioRawFrame(audio=tone(320), sample_rate=16000, num_channels=1))
        first = json.loads(await s.serialize(InterruptionFrame()))
        await s.serialize(OutputAudioRawFrame(audio=tone(320), sample_rate=16000, num_channels=1))
        second = json.loads(await s.serialize(InterruptionFrame()))
        self.assertEqual(first, {"type": "clear", "epoch": 1})
        self.assertEqual(second, {"type": "clear", "epoch": 2})
        self.assertEqual(s.epoch, 2)

    async def test_an_interruption_with_the_bot_silent_sends_nothing(self):
        # Pipecat's user aggregator interrupts on every user turn start; brisvox clears only on
        # a barge-in, so a turn start before any bot audio is not a `clear`.
        clock = FakeClock()
        s = await self.serializer(clock=clock)
        self.assertIsNone(await s.serialize(InterruptionFrame()))
        self.assertEqual(s.epoch, 0)
        # A clear empties the playout: a second interruption right after it sends nothing.
        await s.serialize(OutputAudioRawFrame(audio=tone(320), sample_rate=16000, num_channels=1))
        self.assertIsNotNone(await s.serialize(InterruptionFrame()))
        self.assertIsNone(await s.serialize(InterruptionFrame()))
        self.assertEqual(s.epoch, 1)

    async def test_an_interruption_after_the_reply_played_out_sends_nothing(self):
        clock = FakeClock()
        s = await self.serializer(clock=clock)
        # 1 s of bot audio sent in a burst plays until t=1.0; the grace keeps it until t=1.2.
        for _ in range(50):
            await s.serialize(OutputAudioRawFrame(audio=tone(320), sample_rate=16000, num_channels=1))
        clock.now = 1.1
        self.assertIsNotNone(await s.serialize(InterruptionFrame()))
        for _ in range(50):
            await s.serialize(OutputAudioRawFrame(audio=tone(320), sample_rate=16000, num_channels=1))
        clock.now = 1.1 + 1.0 + PLAYOUT_GRACE_SECS + 0.01
        self.assertIsNone(await s.serialize(InterruptionFrame()))
        self.assertEqual(s.epoch, 1)

    async def test_mulaw_audio_counts_as_playing_for_its_duration(self):
        clock = FakeClock()
        s = await self.serializer(rate=8000, encoding="mulaw", clock=clock)
        # 20 ms at 8 kHz: playing until 0.02 + grace.
        await s.serialize(OutputAudioRawFrame(audio=tone(160), sample_rate=8000, num_channels=1))
        clock.now = 0.02 + PLAYOUT_GRACE_SECS + 0.001
        self.assertIsNone(await s.serialize(InterruptionFrame()))

    async def test_the_end_of_the_pipeline_is_one_stop(self):
        s = await self.serializer()
        self.assertEqual(json.loads(await s.serialize(EndFrame())), {"type": "stop", "reason": "ended"})
        self.assertIsNone(await s.serialize(CancelFrame()))

    async def test_a_cancel_is_a_stop_too(self):
        s = await self.serializer()
        self.assertEqual(json.loads(await s.serialize(CancelFrame()))["type"], "stop")

    async def test_bot_audio_at_the_call_rate_passes_unchanged(self):
        s = await self.serializer()
        pcm = tone(320)
        out = await s.serialize(OutputAudioRawFrame(audio=pcm, sample_rate=16000, num_channels=1))
        self.assertEqual(out, pcm)

    async def test_bot_audio_at_another_rate_is_resampled(self):
        s = await self.serializer()
        out = b""
        for _ in range(50):
            out += await s.serialize(OutputAudioRawFrame(audio=tone(480), sample_rate=24000, num_channels=1))
        # 50 x 20 ms at 24 kHz is 1 s, 16000 samples at 16 kHz. Pipecat's soxr stream resampler
        # releases audio in bursts of about 100 ms, so up to that much is still held back.
        self.assertGreaterEqual(len(out) / 2, 16000 - 1800)
        self.assertLessEqual(len(out) / 2, 16000)

    async def test_mulaw_calls_get_one_byte_per_sample(self):
        s = await self.serializer(rate=8000, encoding="mulaw")
        out = await s.serialize(OutputAudioRawFrame(audio=tone(160), sample_rate=8000, num_channels=1))
        self.assertEqual(len(out), 160)

    async def test_messages_pass_as_json_but_rtvi_does_not(self):
        s = await self.serializer()
        out = await s.serialize(OutputTransportMessageUrgentFrame(message={"type": "custom"}))
        self.assertEqual(json.loads(out), {"type": "custom"})
        rtvi = OutputTransportMessageUrgentFrame(message={"label": "rtvi-ai", "type": "x"})
        self.assertIsNone(await s.serialize(rtvi))

    async def test_client_audio_becomes_input_frames_at_the_pipeline_rate(self):
        s = await self.serializer()
        pcm = tone(320)
        frame = await s.deserialize(pcm)
        self.assertIsInstance(frame, InputAudioRawFrame)
        self.assertEqual((frame.audio, frame.sample_rate, frame.num_channels), (pcm, 16000, 1))

    async def test_mulaw_8k_client_audio_is_decoded_and_resampled(self):
        s = await self.serializer(rate=8000, encoding="mulaw")
        total = 0
        for _ in range(50):
            frame = await s.deserialize(b"\x7f" * 160)
            if frame is not None:
                self.assertEqual(frame.sample_rate, 16000)
                total += len(frame.audio)
        # 1 s of 8 kHz mu-law is 16000 samples at 16 kHz, minus what the resampler holds back.
        self.assertGreaterEqual(total / 2, 16000 - 1800)
        self.assertLessEqual(total / 2, 16000)

    async def test_bad_audio_is_dropped(self):
        s = await self.serializer()
        self.assertIsNone(await s.deserialize(b""))
        self.assertIsNone(await s.deserialize(b"\x00\x01\x02"))
        self.assertIsNone(await s.deserialize(b"\x00" * (64 * 1024 + 2)))

    async def test_client_stop_cancels_the_pipeline(self):
        s = await self.serializer()
        self.assertIsInstance(await s.deserialize('{"type":"stop"}'), CancelWorkerFrame)

    async def test_dtmf_digits(self):
        s = await self.serializer()
        frame = await s.deserialize('{"type":"dtmf","digit":"5"}')
        self.assertIsInstance(frame, InputDTMFFrame)
        self.assertEqual(frame.button.value, "5")
        # A-D are protocol digits without a Pipecat keypad entry.
        self.assertIsNone(await s.deserialize('{"type":"dtmf","digit":"A"}'))

    async def test_marks_unknown_types_and_garbage_are_ignored(self):
        s = await self.serializer()
        for text in ['{"type":"mark","id":"m"}', '{"type":"future"}', "nope", "[]", start()]:
            with self.subTest(text=text):
                self.assertIsNone(await s.deserialize(text))


if __name__ == "__main__":
    unittest.main()
