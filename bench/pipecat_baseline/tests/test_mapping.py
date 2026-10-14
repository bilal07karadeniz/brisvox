"""Agent file -> Pipecat settings, and the brisvox defaults it assumes, checked against Rust."""

import hashlib
import re
import sys
import tomllib
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))

import agent_mapping as m  # noqa: E402


def rust(path: str) -> str:
    return (REPO / path).read_text(encoding="utf-8")


def bench_agent() -> dict:
    with open(HERE / "agent.toml", "rb") as f:
        return tomllib.load(f)


class BenchAgent(unittest.TestCase):
    def test_the_bench_agent_maps_to_the_documented_settings(self):
        s = m.load(HERE / "agent.toml")
        self.assertEqual((s.name, s.language), ("bench-en", "en"))
        self.assertEqual(s.vad, m.VadSettings(0.5, 0.3, 0.1, None))
        self.assertEqual(s.turn, m.TurnSettings("smart_turn", 3.0, 0.0, 0))
        self.assertEqual(
            s.stt,
            m.SttSettings(
                model="nova-3",
                language="en",
                endpointing_ms=300,
                utterance_end_ms=None,
                smart_format=True,
                punctuate=False,
                interim_results=True,
                keyterms=("Example Shop",),
                api_key_env="DEEPGRAM_API_KEY",
            ),
        )
        self.assertEqual(s.llm.model, "gpt-6-luna")
        self.assertEqual(s.llm.reasoning_effort, "low")
        self.assertEqual(s.llm.max_output_tokens, 400)
        self.assertIsNone(s.llm.service_tier)
        self.assertTrue(s.llm.system_prompt.strip().startswith("You are the voice assistant"))
        self.assertEqual(
            s.tts,
            m.TtsSettings(
                model="sonic-3.6",
                voice="replace-with-a-cartesia-voice-id",
                language="en",
                sample_rate=24000,
                max_buffer_delay_ms=0,
                api_key_env="CARTESIA_API_KEY",
            ),
        )
        self.assertEqual(
            s.api_key_envs(), ("DEEPGRAM_API_KEY", "OPENAI_API_KEY", "CARTESIA_API_KEY")
        )

    def test_the_bench_agent_is_the_example_agent_without_tools_and_speculation(self):
        with open(REPO / "examples/agents/support-en.toml", "rb") as f:
            example = tomllib.load(f)
        bench = bench_agent()
        for section in ("turn", "interruption", "stt", "llm", "tts"):
            with self.subTest(section=section):
                left = dict(example[section])
                right = dict(bench[section])
                left.pop("max_tool_rounds", None)  # tool rounds: no tools in the bench agent
                self.assertEqual(left, right)
        self.assertEqual(example["agent"]["language"], bench["agent"]["language"])
        self.assertTrue(example["agent"]["tools"])
        self.assertNotIn("tools", bench["agent"])
        self.assertFalse(bench["speculation"]["enabled"])

    def test_the_example_agent_with_tools_is_refused(self):
        with self.assertRaisesRegex(m.MappingError, "tools"):
            m.load(REPO / "examples/agents/support-en.toml")


class Mapping(unittest.TestCase):
    def doc(self, **sections) -> dict:
        doc = bench_agent()
        for key, value in sections.items():
            doc[key] = value
        return doc

    def test_interruption_off_keeps_the_brisvox_vad_start(self):
        s = m.map_agent(self.doc(interruption={"preset": "off"}))
        self.assertAlmostEqual(s.vad.start_secs, 0.064)
        self.assertFalse(any("backchannels" in n for n in s.notes))

    def test_min_speech_drives_the_vad_start(self):
        s = m.map_agent(self.doc(interruption={"min_speech_ms": 500}))
        self.assertAlmostEqual(s.vad.start_secs, 0.5)

    def test_min_words_selects_the_min_words_start_strategy(self):
        s = m.map_agent(self.doc(interruption={"min_words": 2}))
        self.assertEqual(s.turn.min_words, 2)

    def test_fixed_silence(self):
        # brisvox counts the silence from its start; Pipecat's timer starts at the VAD stop
        # (100 ms in), so the timeout is the silence minus the VAD stop.
        s = m.map_agent(self.doc(turn={"policy": "fixed_silence", "silence_ms": 700}))
        self.assertEqual(s.turn, m.TurnSettings("speech_timeout", 0.0, 0.6, 0))
        self.assertAlmostEqual(s.vad.stop_secs + s.turn.speech_timeout_secs, 0.7)
        with self.assertRaises(m.MappingError):
            m.map_agent(self.doc(turn={"policy": "fixed_silence"}))

    def test_fixed_silence_follows_a_vad_stop_override(self):
        turn = {"policy": "fixed_silence", "silence_ms": 500}
        s = m.map_agent(self.doc(turn=turn), vad_stop_ms=200)
        self.assertAlmostEqual(s.turn.speech_timeout_secs, 0.3)
        # A silence shorter than the VAD stop ends the turn at the VAD stop.
        s = m.map_agent(self.doc(turn=turn), vad_stop_ms=800)
        self.assertEqual(s.turn.speech_timeout_secs, 0.0)

    def test_the_fallback_silence_is_smart_turns_stop(self):
        s = m.map_agent(self.doc(turn={"policy": "adaptive", "fallback_silence_ms": 1500}))
        self.assertAlmostEqual(s.turn.smart_turn_stop_secs, 1.5)

    def test_overrides(self):
        s = m.map_agent(bench_agent(), vad_start_ms=200, vad_stop_ms=200, vad_min_volume=0.0)
        self.assertEqual(s.vad, m.VadSettings(0.5, 0.2, 0.2, 0.0))

    def test_speculation_is_noted_not_mapped(self):
        s = m.map_agent(self.doc(speculation={"enabled": True}))
        self.assertTrue(any(n.startswith("speculation.enabled") for n in s.notes))

    def test_reasoning_effort_defaults_to_none_like_the_brisvox_adapter(self):
        doc = bench_agent()
        del doc["llm"]["openai"]["reasoning_effort"]
        self.assertEqual(m.map_agent(doc).llm.reasoning_effort, "none")
        doc["llm"]["openai"]["reasoning_effort"] = "disabled"
        self.assertEqual(m.map_agent(doc).llm.reasoning_effort, "none")

    def test_omitted_models_take_the_brisvox_defaults(self):
        doc = bench_agent()
        del doc["stt"]["deepgram"]["model"]
        doc["tts"].pop("sample_rate")
        s = m.map_agent(doc)
        self.assertEqual(
            (s.stt.model, s.llm.model, s.tts.model, s.tts.sample_rate),
            ("nova-3", "gpt-6-luna", "sonic-3.6", 24000),
        )

    def test_endpointing_zero_is_off(self):
        doc = bench_agent()
        doc["stt"]["deepgram"]["endpointing_ms"] = 0
        self.assertIsNone(m.map_agent(doc).stt.endpointing_ms)

    def test_tts_language_is_the_primary_subtag(self):
        doc = bench_agent()
        doc["agent"]["language"] = "en-US"
        s = m.map_agent(doc)
        self.assertEqual((s.stt.language, s.tts.language), ("en-US", "en"))

    def test_unsupported_setups_are_refused(self):
        cases = {
            "stt": {"provider": "gladia", "api_key_env": "X"},
            "llm": {"provider": "anthropic", "api_key_env": "X"},
            "tts": {"provider": "elevenlabs", "api_key_env": "X", "voice": "v"},
            "turn": {"policy": "magic"},
            "interruption": {"preset": "custom"},
        }
        for section, value in cases.items():
            with self.subTest(section=section):
                with self.assertRaises(m.MappingError):
                    m.map_agent(self.doc(**{section: value}))
        doc = bench_agent()
        doc["llm"]["openai"]["base_url"] = "https://example.invalid"
        with self.assertRaises(m.MappingError):
            m.map_agent(doc)


def live_agent() -> dict:
    with open(HERE / "agent_live_en.toml", "rb") as f:
        return tomllib.load(f)


class OpenAiCompatible(unittest.TestCase):
    """The OpenRouter agent of the live comparison."""

    def test_the_live_agent_maps_to_a_chat_completions_server(self):
        s = m.map_agent(live_agent())
        self.assertEqual(
            (s.llm.provider, s.llm.base_url, s.llm.model, s.llm.max_output_tokens),
            ("openai_compatible", "https://openrouter.ai/api/v1", "openai/gpt-6-luna", 120),
        )
        self.assertEqual(s.llm.reasoning_effort, "none")
        self.assertEqual(s.llm.api_key_env, "OPENAI_COMPATIBLE_API_KEY")
        self.assertEqual(
            s.api_key_envs(),
            ("DEEPGRAM_API_KEY", "OPENAI_COMPATIBLE_API_KEY", "CARTESIA_API_KEY"),
        )
        self.assertEqual(s.turn.policy, "smart_turn")
        self.assertTrue(any("openai_compatible" in n for n in s.notes))

    def test_the_aura_agent_maps_to_deepgram_websocket_tts(self):
        with open(HERE / "agent_live_en_aura.toml", "rb") as f:
            s = m.map_agent(tomllib.load(f))
        self.assertEqual(
            (s.tts.provider, s.tts.voice, s.tts.model, s.tts.sample_rate, s.tts.speed),
            ("deepgram_aura", "aura-2-thalia-en", "aura-2-thalia-en", 24000, None),
        )
        self.assertEqual(s.tts.api_key_env, "DEEPGRAM_API_KEY")
        self.assertEqual(s.llm.provider_routing, {"order": ["openai"], "allow_fallbacks": False})
        self.assertTrue(any("deepgram_aura" in n for n in s.notes))

    def test_openrouter_routing_maps_to_the_provider_object(self):
        s = m.map_agent(live_agent())
        self.assertEqual(s.llm.provider_routing, {"order": ["openai"], "allow_fallbacks": False})
        doc = live_agent()
        del doc["llm"]["openai_compatible"]["openrouter_order"]
        with self.assertRaises(m.MappingError):  # the fallbacks flag alone is a mistake
            m.map_agent(doc)
        del doc["llm"]["openai_compatible"]["openrouter_allow_fallbacks"]
        self.assertIsNone(m.map_agent(doc).llm.provider_routing)

    def test_the_cli_overrides_replace_url_and_model(self):
        s = m.map_agent(live_agent(), llm_base_url="http://127.0.0.1:9/v1", llm_model="x/y")
        self.assertEqual((s.llm.base_url, s.llm.model), ("http://127.0.0.1:9/v1", "x/y"))

    def test_a_keyless_server_has_no_llm_key_env(self):
        doc = live_agent()
        del doc["llm"]["api_key_env"]
        s = m.map_agent(doc)
        self.assertIsNone(s.llm.api_key_env)
        self.assertEqual(s.api_key_envs(), ("DEEPGRAM_API_KEY", "CARTESIA_API_KEY"))

    def test_incomplete_or_unsupported_compatible_setups_are_refused(self):
        doc = live_agent()
        del doc["llm"]["openai_compatible"]["model"]
        with self.assertRaises(m.MappingError):
            m.map_agent(doc)
        doc = live_agent()
        doc["llm"]["openai_compatible"] = {"preset": "groq"}
        with self.assertRaises(m.MappingError):
            m.map_agent(doc)
        # the overrides belong to the compatible provider only
        with self.assertRaises(m.MappingError):
            m.map_agent(bench_agent(), llm_model="x/y")


class BrisvoxDefaults(unittest.TestCase):
    """The constants of agent_mapping.py are brisvox's; a changed default must fail here."""

    def test_provider_defaults(self):
        openai = rust("crates/brisvox-providers/src/openai/config.rs")
        self.assertIn(f'pub const DEFAULT_MODEL: &str = "{m.BRISVOX_OPENAI_MODEL}";', openai)
        self.assertIn("reasoning_effort: Some(ReasoningEffort::Disabled)", openai)
        self.assertIn('Self::Disabled => "none"', openai)
        cartesia = rust("crates/brisvox-providers/src/cartesia/mod.rs")
        self.assertIn(f'pub const DEFAULT_MODEL: &str = "{m.BRISVOX_CARTESIA_MODEL}";', cartesia)
        self.assertIn(f"max_buffer_delay_ms: {m.BRISVOX_CARTESIA_MAX_BUFFER_DELAY_MS},", cartesia)
        deepgram = rust("crates/brisvox-providers/src/deepgram/config.rs")
        expected = [
            f'model: "{m.BRISVOX_DEEPGRAM_MODEL}".to_owned(),',
            f"interim_results: {str(m.BRISVOX_DEEPGRAM_INTERIM_RESULTS).lower()},",
            f"endpointing: Endpointing::Ms({m.BRISVOX_DEEPGRAM_ENDPOINTING_MS}),",
            f"smart_format: {str(m.BRISVOX_DEEPGRAM_SMART_FORMAT).lower()},",
            f"punctuate: {str(m.BRISVOX_DEEPGRAM_PUNCTUATE).lower()},",
        ]
        for line in expected:
            with self.subTest(line=line):
                self.assertIn(line, deepgram)

    def test_vad_defaults(self):
        vad = rust("crates/brisvox-inference/src/vad.rs")
        block = vad[vad.index("impl Default for VadParams") :][:900]
        self.assertIn(f"threshold: {m.BRISVOX_VAD_THRESHOLD},", block)
        self.assertIn(f"min_speech: Duration::from_millis({m.BRISVOX_VAD_MIN_SPEECH_MS}),", block)
        self.assertIn(f"min_silence: Duration::from_millis({m.BRISVOX_VAD_MIN_SILENCE_MS}),", block)
        # The server runs these defaults unchanged.
        self.assertIn(
            "params: VadParams::default(),", rust("crates/brisvox-engine/src/vad.rs")
        )

    def test_turn_and_interruption_defaults(self):
        turn = rust("crates/brisvox-agent/src/turn.rs")
        self.assertIn(
            f"pub const DEFAULT_FALLBACK_SILENCE: Duration = "
            f"Duration::from_secs({m.BRISVOX_FALLBACK_SILENCE_MS // 1000});",
            turn,
        )
        config = rust("crates/brisvox-engine/src/config.rs")
        block = config[config.index("pub const RECOMMENDED: Self") :][:600]
        self.assertIn(
            f"min_speech: Duration::from_millis({m.BRISVOX_RECOMMENDED_MIN_SPEECH_MS}),", block
        )
        self.assertIn(
            f"echo_guard: Duration::from_millis({m.BRISVOX_RECOMMENDED_ECHO_GUARD_MS}),", block
        )
        self.assertIn("min_words: 0,", block)
        interrupt = rust("crates/brisvox-agent/src/interrupt.rs")
        self.assertIn(
            "pub const DEFAULT_VAD_HANGOVER: Duration = "
            f"Duration::from_millis({m.BRISVOX_VAD_MIN_SILENCE_MS});",
            interrupt,
        )

    def test_pipecat_runs_the_same_smart_turn_model(self):
        import pipecat.audio.turn.smart_turn.data as data

        model = Path(data.__file__).parent / "smart-turn-v3.2-cpu.onnx"
        source = rust("crates/brisvox-inference/src/smart_turn/mod.rs")
        size = int(re.search(r"pub const MODEL_BYTES: u64 = ([\d_]+);", source)[1].replace("_", ""))
        self.assertIn('pub const MODEL_FILE: &str = "smart-turn-v3.2-cpu.onnx";', source)
        self.assertEqual(model.stat().st_size, size)
        digest = hashlib.sha256(model.read_bytes()).hexdigest()
        # brisvox pins the file by SHA-256 `2bb02631...e4f` (module docs, docs/providers).
        self.assertTrue(digest.startswith("2bb02631") and digest.endswith("e4f"), digest)
        self.assertIn("pub const THRESHOLD: f32 = 0.5;", source)


if __name__ == "__main__":
    unittest.main()
