//! Answering-machine detection (AMD) for outbound calls: the provider-neutral
//! configuration, its rendering as Twilio request parameters and as Telnyx dial fields, and the
//! parsers that turn provider callbacks into an [`AnsweredBy`]. Facts:
//! `docs/providers/call-control.md`, 1.7 (Twilio) and 2.4 (Telnyx).
//!
//! brisvox does not serve webhooks (`docs/providers/twilio.md`, scope boundary). The
//! application's HTTP app receives the provider's callback and hands the raw body to
//! [`parse_twilio_amd_callback`] or [`parse_telnyx_amd_event`]; the result goes to the session's
//! [`VoicemailDriver`](brisvox_agent::telephony::voicemail::VoicemailDriver).
//! [`AmdConfig::twilio_params`] and [`AmdConfig::telnyx_fields`] are what the originating call
//! adds to its Calls-create / Dial request (and what the warm-transfer agent-leg screening of
//! the bindings uses).

use std::time::Duration;

use brisvox_agent::AnsweredBy;
use serde_json::{Map, Value, json};

use super::error::{Error, Result};
use super::http::form_decode;

/// How hard the detector works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmdMode {
    /// Decide human or machine as early as possible (Twilio `Enable`, Telnyx `detect`). No wait
    /// for the end of a greeting: use it to connect people and hang up on machines.
    Detect,
    /// For a machine, wait until its greeting ends (Twilio `DetectMessageEnd`, Telnyx
    /// `greeting_end`). The mode for leaving a message.
    DetectMessageEnd,
}

/// Provider-neutral AMD settings. Unset (`None`) fields are not sent, so the provider's defaults
/// apply. The Telnyx mapping of the timing fields is an approximation of Twilio's meaning (see
/// [`Self::telnyx_fields`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmdConfig {
    /// Detect early, or wait for the end of the greeting.
    pub mode: AmdMode,
    /// Longest analysis. Twilio `MachineDetectionTimeout` (3 to 59 s, default 30 s; "30 seconds
    /// is frequently not enough" for business voicemail).
    pub timeout: Option<Duration>,
    /// Initial silence after which the answer is `unknown` (Twilio 2 to 10 s, default 5 s).
    pub silence_timeout: Option<Duration>,
    /// Length of speech after which the answer is a machine (Twilio 1 to 6 s, default 2.4 s).
    pub speech_threshold: Option<Duration>,
    /// Silence that ends a greeting (Twilio 0.5 to 5 s, default 1.2 s).
    pub speech_end_threshold: Option<Duration>,
    /// Twilio only: do not block the call for the verdict; the result is posted to
    /// [`callback_url`](Self::callback_url) (`AsyncAmd`). Twilio says to start Media Streams
    /// only after the callback, so this is not combined with a bidirectional stream by default.
    pub async_amd: bool,
    /// Twilio only: where the async result is posted (`AsyncAmdStatusCallback`). Required with
    /// `async_amd`.
    pub callback_url: Option<String>,
    /// Telnyx only: the `premium` detector (a separate price).
    pub premium: bool,
}

impl AmdConfig {
    /// Settings for `mode` with every tunable at the provider's default.
    pub fn new(mode: AmdMode) -> Self {
        Self {
            mode,
            timeout: None,
            silence_timeout: None,
            speech_threshold: None,
            speech_end_threshold: None,
            async_amd: false,
            callback_url: None,
            premium: false,
        }
    }

    /// Whether Twilio reports the end of a machine's greeting with this configuration (only
    /// `DetectMessageEnd` does; `Enable` ends at `machine_start`). When it does not, a machine
    /// verdict is the last event of the call and the leave-message flow must use
    /// [`VoicemailPolicy::detect_only`](brisvox_agent::telephony::voicemail::VoicemailPolicy::detect_only)
    /// (hang up on machines) instead of waiting for a greeting end that never comes.
    pub fn twilio_provides_greeting_end(&self) -> bool {
        self.mode == AmdMode::DetectMessageEnd
    }

    /// Whether Telnyx reports the end of a machine's greeting with this configuration
    /// (`greeting_end` and `premium` send a greeting event; `detect` only
    /// `call.machine.detection.ended`). See [`Self::twilio_provides_greeting_end`] for what to do
    /// when it does not.
    pub fn telnyx_provides_greeting_end(&self) -> bool {
        self.premium || self.mode == AmdMode::DetectMessageEnd
    }

    /// Checks the documented ranges.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] naming the field.
    pub fn validate(&self) -> Result<()> {
        let ms = |d: Duration| d.as_millis();
        let check = |value: Option<Duration>, lo: u128, hi: u128, name: &str| -> Result<()> {
            match value {
                Some(d) if !(lo..=hi).contains(&ms(d)) => Err(Error::InvalidArgument(format!(
                    "AMD {name} must be {lo} to {hi} ms"
                ))),
                _ => Ok(()),
            }
        };
        check(self.timeout, 3_000, 59_000, "timeout")?;
        check(self.silence_timeout, 2_000, 10_000, "silence_timeout")?;
        check(self.speech_threshold, 1_000, 6_000, "speech_threshold")?;
        check(
            self.speech_end_threshold,
            500,
            5_000,
            "speech_end_threshold",
        )?;
        if self.async_amd && self.callback_url.as_deref().is_none_or(str::is_empty) {
            return Err(Error::InvalidArgument(
                "async AMD needs a callback_url: the result is posted there".to_owned(),
            ));
        }
        Ok(())
    }

    /// The Twilio request parameters (Calls create, Participants create): `MachineDetection`,
    /// `MachineDetectionTimeout` (seconds), the three thresholds (milliseconds), `AsyncAmd` and
    /// `AsyncAmdStatusCallback`. `premium` is ignored.
    ///
    /// # Errors
    /// As [`Self::validate`].
    pub fn twilio_params(&self) -> Result<Vec<(&'static str, String)>> {
        self.validate()?;
        let mut params = vec![(
            "MachineDetection",
            match self.mode {
                AmdMode::Detect => "Enable",
                AmdMode::DetectMessageEnd => "DetectMessageEnd",
            }
            .to_owned(),
        )];
        if let Some(d) = self.timeout {
            params.push(("MachineDetectionTimeout", d.as_secs().to_string()));
        }
        if let Some(d) = self.speech_threshold {
            params.push(("MachineDetectionSpeechThreshold", d.as_millis().to_string()));
        }
        if let Some(d) = self.speech_end_threshold {
            params.push((
                "MachineDetectionSpeechEndThreshold",
                d.as_millis().to_string(),
            ));
        }
        if let Some(d) = self.silence_timeout {
            params.push(("MachineDetectionSilenceTimeout", d.as_millis().to_string()));
        }
        if self.async_amd {
            params.push(("AsyncAmd", "true".to_owned()));
            if let Some(url) = &self.callback_url {
                params.push(("AsyncAmdStatusCallback", url.clone()));
            }
        }
        Ok(params)
    }

    /// The Telnyx fields for a Dial or Transfer request: `answering_machine_detection` and, when
    /// any tunable is set, `answering_machine_detection_config`.
    ///
    /// The mapping from the neutral fields is an approximation (UNVERIFIED equivalence):
    /// `timeout` to `total_analysis_time_millis` (`greeting_total_analysis_time_millis` for
    /// [`AmdMode::DetectMessageEnd`] as well), `silence_timeout` to `initial_silence_millis`,
    /// `speech_threshold` to `greeting_duration_millis`, `speech_end_threshold` to
    /// `after_greeting_silence_millis`. With `premium` only `total_analysis_time_millis` and
    /// `greeting_duration_millis` apply (the Dial page). `async_amd` and `callback_url` are
    /// Twilio concepts and are ignored: Telnyx delivers the result as webhooks.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for an out-of-range tunable.
    pub fn telnyx_fields(&self) -> Result<Vec<(&'static str, Value)>> {
        let mut checked = self.clone();
        checked.async_amd = false;
        checked.validate()?;
        let mode = if self.premium {
            "premium"
        } else {
            match self.mode {
                AmdMode::Detect => "detect",
                AmdMode::DetectMessageEnd => "greeting_end",
            }
        };
        let mut config = Map::new();
        let mut put = |key: &str, value: Option<Duration>| {
            if let Some(d) = value {
                config.insert(
                    key.to_owned(),
                    json!(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                );
            }
        };
        put("total_analysis_time_millis", self.timeout);
        put("greeting_duration_millis", self.speech_threshold);
        if !self.premium {
            if self.mode == AmdMode::DetectMessageEnd {
                put("greeting_total_analysis_time_millis", self.timeout);
            }
            put("initial_silence_millis", self.silence_timeout);
            put("after_greeting_silence_millis", self.speech_end_threshold);
        }
        let mut fields = vec![("answering_machine_detection", json!(mode))];
        if !config.is_empty() {
            fields.push(("answering_machine_detection_config", Value::Object(config)));
        }
        Ok(fields)
    }
}

/// Maps a Twilio `AnsweredBy` value. An unknown string is [`AnsweredBy::Unknown`].
pub fn answered_by_from_twilio(value: &str) -> AnsweredBy {
    match value.trim() {
        "human" => AnsweredBy::Human,
        "machine_start" => AnsweredBy::MachineStart,
        "machine_end_beep" => AnsweredBy::MachineEndBeep,
        "machine_end_silence" => AnsweredBy::MachineEndSilence,
        "machine_end_other" => AnsweredBy::MachineEndOther,
        "fax" => AnsweredBy::Fax,
        _ => AnsweredBy::Unknown,
    }
}

/// The AMD result Twilio posts to `AsyncAmdStatusCallback` (and, for the synchronous mode, the
/// `AnsweredBy` field of the request to the call's `Url`: the field name there is UNVERIFIED, so
/// this parser accepts the same form with or without the other fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwilioAmdResult {
    /// The call the result is for.
    pub call_sid: String,
    /// The verdict.
    pub answered_by: AnsweredBy,
    /// How long the detection took (`MachineDetectionDuration`), when sent.
    pub detection: Option<Duration>,
}

/// Parses a Twilio AMD callback body (`application/x-www-form-urlencoded`).
///
/// # Errors
/// [`Error::Protocol`] if `CallSid` or `AnsweredBy` is missing.
pub fn parse_twilio_amd_callback(body: &str) -> Result<TwilioAmdResult> {
    let fields = form_decode(body);
    let get = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let call_sid = get("CallSid").filter(|v| !v.is_empty());
    let answered_by = get("AnsweredBy");
    match (call_sid, answered_by) {
        (Some(call_sid), Some(answered_by)) => Ok(TwilioAmdResult {
            call_sid: call_sid.to_owned(),
            answered_by: answered_by_from_twilio(answered_by),
            detection: get("MachineDetectionDuration")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_millis),
        }),
        _ => Err(Error::Protocol {
            provider: "twilio",
            detail: "AMD callback without CallSid or AnsweredBy".to_owned(),
        }),
    }
}

/// An AMD webhook of Telnyx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelnyxAmdEvent {
    /// `data.id`, the webhook's id. Telnyx may deliver a webhook more than once: dedupe on it.
    pub event_id: Option<String>,
    /// `data.event_type`.
    pub event_type: String,
    /// The leg the result is for.
    pub call_control_id: String,
    /// The verdict, mapped.
    pub answered_by: AnsweredBy,
    /// `data.payload.result` as sent (for logs).
    pub result: String,
}

/// Parses a Telnyx webhook body. Returns `Ok(None)` for events that are not an AMD verdict
/// (`call.machine.premium.call_screening.detected` is one of them: it is not a verdict on its
/// own).
///
/// Mapping (`docs/providers/call-control.md`, 2.4):
/// - detection ended: `human` to Human, `machine` to MachineStart (the greeting may still be
///   playing; wait for the greeting event), `not_sure` to Unknown;
/// - greeting ended: `beep_detected` to MachineEndBeep, `ended` and `no_beep_detected` to
///   MachineEndOther, `not_sure` to Unknown;
/// - premium detection: `human_residence`, `human_business` to Human, `machine` to MachineStart,
///   `fax_detected` to Fax, `silence` and `not_sure` to Unknown.
///
/// A result string not in the documentation maps to Unknown. The premium payload schemas were
/// not fetched; only the values of the guide are known (UNVERIFIED beyond them).
///
/// # Errors
/// [`Error::Protocol`] if the body is not JSON or an AMD event lacks `call_control_id` or
/// `result`.
pub fn parse_telnyx_amd_event(body: &[u8]) -> Result<Option<TelnyxAmdEvent>> {
    let bad = |detail: &str| Error::Protocol {
        provider: "telnyx",
        detail: detail.to_owned(),
    };
    let root: Value =
        serde_json::from_slice(body).map_err(|_| bad("the webhook body is not JSON"))?;
    let data = root.get("data").ok_or_else(|| bad("no data member"))?;
    let event_type = data
        .get("event_type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("no data.event_type"))?;
    let greeting = match event_type {
        "call.machine.detection.ended" | "call.machine.premium.detection.ended" => false,
        "call.machine.greeting.ended" | "call.machine.premium.greeting.ended" => true,
        _ => return Ok(None),
    };
    let payload = data.get("payload").ok_or_else(|| bad("no data.payload"))?;
    let call_control_id = payload
        .get("call_control_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| bad("no payload.call_control_id"))?;
    let result = payload
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("no payload.result"))?;
    let answered_by = if greeting {
        match result {
            "beep_detected" => AnsweredBy::MachineEndBeep,
            "ended" | "no_beep_detected" => AnsweredBy::MachineEndOther,
            _ => AnsweredBy::Unknown,
        }
    } else {
        match result {
            "human" | "human_residence" | "human_business" => AnsweredBy::Human,
            "machine" => AnsweredBy::MachineStart,
            "fax_detected" => AnsweredBy::Fax,
            _ => AnsweredBy::Unknown,
        }
    };
    Ok(Some(TelnyxAmdEvent {
        event_id: data.get("id").and_then(Value::as_str).map(str::to_owned),
        event_type: event_type.to_owned(),
        call_control_id: call_control_id.to_owned(),
        answered_by,
        result: result.chars().take(64).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twilio_params_follow_the_documented_names() {
        let mut config = AmdConfig::new(AmdMode::DetectMessageEnd);
        config.timeout = Some(Duration::from_secs(45));
        config.speech_threshold = Some(Duration::from_millis(1800));
        config.speech_end_threshold = Some(Duration::from_millis(1400));
        config.silence_timeout = Some(Duration::from_millis(5000));
        let params = config.twilio_params().unwrap();
        let get = |k: &str| params.iter().find(|p| p.0 == k).map(|p| p.1.as_str());
        assert_eq!(get("MachineDetection"), Some("DetectMessageEnd"));
        assert_eq!(get("MachineDetectionTimeout"), Some("45"));
        assert_eq!(get("MachineDetectionSpeechThreshold"), Some("1800"));
        assert_eq!(get("MachineDetectionSpeechEndThreshold"), Some("1400"));
        assert_eq!(get("MachineDetectionSilenceTimeout"), Some("5000"));
        assert_eq!(get("AsyncAmd"), None);

        let mut async_cfg = AmdConfig::new(AmdMode::Detect);
        async_cfg.async_amd = true;
        assert!(async_cfg.twilio_params().is_err());
        async_cfg.callback_url = Some("https://app.example/amd".to_owned());
        let params = async_cfg.twilio_params().unwrap();
        assert!(params.contains(&("MachineDetection", "Enable".to_owned())));
        assert!(params.contains(&("AsyncAmd", "true".to_owned())));
        assert!(params.contains(&(
            "AsyncAmdStatusCallback",
            "https://app.example/amd".to_owned()
        )));
    }

    #[test]
    fn ranges_are_checked() {
        let mut config = AmdConfig::new(AmdMode::Detect);
        config.timeout = Some(Duration::from_secs(60));
        assert!(config.validate().is_err());
        config.timeout = Some(Duration::from_secs(2));
        assert!(config.validate().is_err());
        config.timeout = Some(Duration::from_secs(59));
        config.speech_end_threshold = Some(Duration::from_millis(100));
        assert!(config.validate().is_err());
    }

    /// Each tunable on its own: `lo - 1` and `hi + 1` are rejected, `lo` and `hi` accepted (the
    /// ranges are inclusive, in milliseconds). The error names the field.
    #[test]
    fn every_range_is_inclusive_and_checked_on_its_own() {
        type Set = fn(&mut AmdConfig, Option<Duration>);
        let fields: [(&str, Set, u64, u64); 4] = [
            ("timeout", |c, d| c.timeout = d, 3_000, 59_000),
            (
                "silence_timeout",
                |c, d| c.silence_timeout = d,
                2_000,
                10_000,
            ),
            (
                "speech_threshold",
                |c, d| c.speech_threshold = d,
                1_000,
                6_000,
            ),
            (
                "speech_end_threshold",
                |c, d| c.speech_end_threshold = d,
                500,
                5_000,
            ),
        ];
        for (name, set, lo, hi) in fields {
            let check = |millis: u64| {
                let mut config = AmdConfig::new(AmdMode::Detect);
                set(&mut config, Some(Duration::from_millis(millis)));
                config.validate()
            };
            assert!(check(lo - 1).is_err(), "{name}: {} ms is too low", lo - 1);
            assert!(check(lo).is_ok(), "{name}: {lo} ms is the lower bound");
            assert!(check(hi).is_ok(), "{name}: {hi} ms is the upper bound");
            assert!(check(hi + 1).is_err(), "{name}: {} ms is too high", hi + 1);
            let message = check(hi + 1).unwrap_err().to_string();
            assert!(message.contains(name), "{message}");
        }
    }

    #[test]
    fn telnyx_fields_map_modes_and_only_send_what_is_set() {
        let plain = AmdConfig::new(AmdMode::DetectMessageEnd)
            .telnyx_fields()
            .unwrap();
        assert_eq!(
            plain,
            vec![("answering_machine_detection", json!("greeting_end"))]
        );
        let detect = AmdConfig::new(AmdMode::Detect).telnyx_fields().unwrap();
        assert_eq!(
            detect,
            vec![("answering_machine_detection", json!("detect"))]
        );

        let mut tuned = AmdConfig::new(AmdMode::DetectMessageEnd);
        tuned.timeout = Some(Duration::from_secs(10));
        tuned.silence_timeout = Some(Duration::from_secs(3));
        let fields = tuned.telnyx_fields().unwrap();
        assert_eq!(
            fields.first(),
            Some(&("answering_machine_detection", json!("greeting_end")))
        );
        let config = fields.get(1).map(|f| &f.1).unwrap();
        assert_eq!(config["total_analysis_time_millis"], 10_000);
        assert_eq!(config["greeting_total_analysis_time_millis"], 10_000);
        assert_eq!(config["initial_silence_millis"], 3_000);

        tuned.premium = true;
        let fields = tuned.telnyx_fields().unwrap();
        assert_eq!(
            fields.first(),
            Some(&("answering_machine_detection", json!("premium")))
        );
        let config = fields.get(1).map(|f| &f.1).unwrap();
        assert_eq!(config["total_analysis_time_millis"], 10_000);
        assert!(config.get("initial_silence_millis").is_none());
    }

    /// All four timers set to distinct values: every Telnyx key carries the right neutral field,
    /// in the non-premium modes and in premium (which only sends two of them).
    #[test]
    fn telnyx_maps_every_timer_to_its_own_key() {
        let tuned = |mode: AmdMode, premium: bool| {
            let mut config = AmdConfig::new(mode);
            config.timeout = Some(Duration::from_millis(11_000));
            config.silence_timeout = Some(Duration::from_millis(3_000));
            config.speech_threshold = Some(Duration::from_millis(2_000));
            config.speech_end_threshold = Some(Duration::from_millis(1_500));
            config.premium = premium;
            let fields = config.telnyx_fields().unwrap();
            let Some((_, Value::Object(map))) = fields.into_iter().nth(1) else {
                panic!("no answering_machine_detection_config");
            };
            map
        };

        let detect = tuned(AmdMode::Detect, false);
        assert_eq!(detect.len(), 4, "{detect:?}");
        assert_eq!(detect["total_analysis_time_millis"], 11_000);
        assert_eq!(detect["initial_silence_millis"], 3_000);
        assert_eq!(detect["greeting_duration_millis"], 2_000);
        assert_eq!(detect["after_greeting_silence_millis"], 1_500);

        let end = tuned(AmdMode::DetectMessageEnd, false);
        assert_eq!(end.len(), 5, "{end:?}");
        assert_eq!(end["total_analysis_time_millis"], 11_000);
        assert_eq!(end["greeting_total_analysis_time_millis"], 11_000);
        assert_eq!(end["initial_silence_millis"], 3_000);
        assert_eq!(end["greeting_duration_millis"], 2_000);
        assert_eq!(end["after_greeting_silence_millis"], 1_500);

        // Premium: only total_analysis_time_millis and greeting_duration_millis apply.
        for mode in [AmdMode::Detect, AmdMode::DetectMessageEnd] {
            let premium = tuned(mode, true);
            assert_eq!(premium.len(), 2, "{premium:?}");
            assert_eq!(premium["total_analysis_time_millis"], 11_000);
            assert_eq!(premium["greeting_duration_millis"], 2_000);
            assert!(premium.get("after_greeting_silence_millis").is_none());
            assert!(premium.get("initial_silence_millis").is_none());
        }
    }

    #[test]
    fn twilio_values_map() {
        for (text, expected) in [
            ("human", AnsweredBy::Human),
            ("machine_start", AnsweredBy::MachineStart),
            ("machine_end_beep", AnsweredBy::MachineEndBeep),
            ("machine_end_silence", AnsweredBy::MachineEndSilence),
            ("machine_end_other", AnsweredBy::MachineEndOther),
            ("fax", AnsweredBy::Fax),
            ("unknown", AnsweredBy::Unknown),
            ("something_new", AnsweredBy::Unknown),
        ] {
            assert_eq!(answered_by_from_twilio(text), expected, "{text}");
        }
    }

    #[test]
    fn twilio_callback_parses() {
        let body = "CallSid=CA123&AccountSid=AC1&AnsweredBy=machine_end_beep&MachineDetectionDuration=4200";
        let result = parse_twilio_amd_callback(body).unwrap();
        assert_eq!(result.call_sid, "CA123");
        assert_eq!(result.answered_by, AnsweredBy::MachineEndBeep);
        assert_eq!(result.detection, Some(Duration::from_millis(4200)));
        assert!(parse_twilio_amd_callback("AnsweredBy=human").is_err());
        assert!(parse_twilio_amd_callback("CallSid=CA1").is_err());
        let no_duration = parse_twilio_amd_callback("CallSid=CA1&AnsweredBy=human").unwrap();
        assert_eq!(no_duration.detection, None);
    }

    fn telnyx(event_type: &str, result: &str) -> Vec<u8> {
        json!({
            "data": {
                "record_type": "event",
                "event_type": event_type,
                "id": "evt-1",
                "payload": { "call_control_id": "v3:abc", "result": result },
            },
            "meta": { "attempt": 1 },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn telnyx_events_map() {
        let cases = [
            ("call.machine.detection.ended", "human", AnsweredBy::Human),
            (
                "call.machine.detection.ended",
                "machine",
                AnsweredBy::MachineStart,
            ),
            (
                "call.machine.detection.ended",
                "not_sure",
                AnsweredBy::Unknown,
            ),
            (
                "call.machine.greeting.ended",
                "beep_detected",
                AnsweredBy::MachineEndBeep,
            ),
            (
                "call.machine.greeting.ended",
                "ended",
                AnsweredBy::MachineEndOther,
            ),
            (
                "call.machine.greeting.ended",
                "not_sure",
                AnsweredBy::Unknown,
            ),
            (
                "call.machine.premium.detection.ended",
                "human_residence",
                AnsweredBy::Human,
            ),
            (
                "call.machine.premium.detection.ended",
                "human_business",
                AnsweredBy::Human,
            ),
            (
                "call.machine.premium.detection.ended",
                "machine",
                AnsweredBy::MachineStart,
            ),
            (
                "call.machine.premium.detection.ended",
                "silence",
                AnsweredBy::Unknown,
            ),
            (
                "call.machine.premium.detection.ended",
                "fax_detected",
                AnsweredBy::Fax,
            ),
            (
                "call.machine.premium.greeting.ended",
                "beep_detected",
                AnsweredBy::MachineEndBeep,
            ),
            (
                "call.machine.premium.greeting.ended",
                "no_beep_detected",
                AnsweredBy::MachineEndOther,
            ),
            (
                "call.machine.detection.ended",
                "brand_new_value",
                AnsweredBy::Unknown,
            ),
        ];
        for (event_type, result, expected) in cases {
            let event = parse_telnyx_amd_event(&telnyx(event_type, result))
                .unwrap()
                .unwrap();
            assert_eq!(event.answered_by, expected, "{event_type} {result}");
            assert_eq!(event.call_control_id, "v3:abc");
            assert_eq!(event.event_id.as_deref(), Some("evt-1"));
        }
    }

    #[test]
    fn telnyx_other_events_and_bad_bodies() {
        assert_eq!(
            parse_telnyx_amd_event(&telnyx("call.answered", "x")).unwrap(),
            None
        );
        assert_eq!(
            parse_telnyx_amd_event(&telnyx(
                "call.machine.premium.call_screening.detected",
                "screening"
            ))
            .unwrap(),
            None
        );
        assert!(parse_telnyx_amd_event(b"not json").is_err());
        assert!(parse_telnyx_amd_event(b"{}").is_err());
        let no_id = json!({"data": {"event_type": "call.machine.detection.ended", "payload": {"result": "human"}}});
        assert!(parse_telnyx_amd_event(no_id.to_string().as_bytes()).is_err());
    }
}
