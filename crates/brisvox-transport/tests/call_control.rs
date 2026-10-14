//! Tests of the call-control adapters against a local mock HTTP server:
//! exact request paths, authentication headers and bodies, the TwiML and JSON of every
//! operation, error mapping, timeouts, the warm-transfer sequences (including rollback and
//! cancellation), and the agent tools running end to end over a bound call. Real sockets, so
//! the real `SystemClock` and short deadlines, except one test that proves the request deadline
//! runs on the injected `Clock` (a clock that runs a thousand times faster than real time).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::too_many_lines,
    clippy::disallowed_methods // real sockets: real time and real sleeps
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use brisvox_agent::context::{Message, ToolCall, ToolCallId};
use brisvox_agent::tools::{EngineConfig, SessionId, ToolEngine, ToolRegistry};
use brisvox_agent::{
    Assembled, Briefing, DtmfString, PhoneNumber, TelephonyErrorKind, TelephonyOps,
    TelephonyPolicy, TransferMode, TransferRequest, register_telephony_tools,
};
use brisvox_core::{Clock, Secret, SystemClock, TurnEpoch};
use brisvox_transport::call_control::{
    BoundCall, CallControl, CallRef, CallerState, Error, SettleFn, StreamRestartHook,
    StreamStopHook, TelnyxCallControl, TelnyxConfig, TwilioAuth, TwilioCallControl, TwilioConfig,
    form_decode, twilio::ResumeStream,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------------------------
// A minimal HTTP/1.1 mock server
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn form(&self) -> Vec<(String, String)> {
        form_decode(&self.body)
    }

    fn form_get(&self, key: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn form_all(&self, key: &str) -> Vec<String> {
        self.form()
            .into_iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v)
            .collect()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

struct Answer {
    status: u16,
    body: String,
    delay: Duration,
    location: Option<String>,
}

fn ok(body: Value) -> Answer {
    status(200, body)
}

fn status(status: u16, body: Value) -> Answer {
    Answer {
        status,
        body: body.to_string(),
        delay: Duration::ZERO,
        location: None,
    }
}

impl Answer {
    fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Adds a `Location` header (a redirect).
    fn redirect_to(mut self, location: &str) -> Self {
        self.location = Some(location.to_owned());
        self
    }
}

struct Mock {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

type Handler = dyn Fn(&Recorded) -> Answer + Send + Sync;

impl Mock {
    async fn start(handler: impl Fn(&Recorded) -> Answer + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let log = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&handler), Arc::clone(&log)));
            }
        });
        Self { addr, requests }
    }

    fn url(&self, prefix: &str) -> String {
        format!("http://{}{prefix}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    async fn wait_for(&self, count: usize, within: Duration) {
        let start = Instant::now();
        while self.requests().len() < count {
            assert!(
                start.elapsed() < within,
                "only {} requests",
                self.requests().len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    handler: Arc<Handler>,
    log: Arc<Mutex<Vec<Recorded>>>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let head_end = loop {
            if let Some(pos) = find(&buf, b"\r\n\r\n") {
                break pos;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split(' ');
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let length = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse::<usize>().ok())
            .unwrap_or(0);
        let total = head_end + 4 + length;
        while buf.len() < total {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body = String::from_utf8_lossy(&buf[head_end + 4..total]).into_owned();
        buf.drain(..total);
        let recorded = Recorded {
            method,
            path,
            headers,
            body,
        };
        log.lock().unwrap().push(recorded.clone());
        let answer = handler(&recorded);
        if !answer.delay.is_zero() {
            tokio::time::sleep(answer.delay).await;
        }
        let location = answer
            .location
            .as_ref()
            .map_or_else(String::new, |l| format!("location: {l}\r\n"));
        let response = format!(
            "HTTP/1.1 {} X\r\ncontent-type: application/json\r\n{location}content-length: {}\r\n\r\n{}",
            answer.status,
            answer.body.len(),
            answer.body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

const ACCOUNT: &str = concat!("AC", "1234567890abcdef1234567890abcdef");
const TOKEN: &str = "tok-SECRET-9f8e7d";
const CALLER_SID: &str = "CA00000000000000000000000000000001";
const AGENT_SID: &str = "CA00000000000000000000000000000002";
const TWILIO_PREFIX: &str = "/2010-04-01";
const KEY: &str = "KEY-SECRET-0123456789";

fn clock() -> brisvox_core::SharedClock {
    SystemClock::shared()
}

fn caller() -> CallRef {
    CallRef::new(CALLER_SID).unwrap()
}

fn number() -> PhoneNumber {
    PhoneNumber::parse("+905551112233").unwrap()
}

fn request(mode: TransferMode, summary: Option<&str>) -> TransferRequest {
    TransferRequest {
        destination: number(),
        destination_name: "sales".to_owned(),
        mode,
        summary: summary.map(str::to_owned),
        reason: None,
        idempotency: 0xabc,
    }
}

fn twilio_config(mock: &Mock) -> TwilioConfig {
    let mut config = TwilioConfig::new(
        ACCOUNT,
        TwilioAuth::AuthToken(Secret::new(TOKEN.to_owned())),
    );
    config.base_url = mock.url(TWILIO_PREFIX);
    config.request_timeout = Duration::from_secs(3);
    config
}

fn twilio(mock: &Mock, tweak: impl FnOnce(&mut TwilioConfig)) -> TwilioCallControl {
    let mut config = twilio_config(mock);
    tweak(&mut config);
    TwilioCallControl::new(config, clock()).unwrap()
}

fn resume() -> ResumeStream {
    ResumeStream {
        url: "wss://agent.example.com/twilio/stream".to_owned(),
        url_for_call: None,
        name: Some("brisvox".to_owned()),
        parameters: vec![("agent".to_owned(), "support-tr".to_owned())],
    }
}

fn update_path(sid: &str) -> String {
    format!("{TWILIO_PREFIX}/Accounts/{ACCOUNT}/Calls/{sid}.json")
}

fn create_path() -> String {
    format!("{TWILIO_PREFIX}/Accounts/{ACCOUNT}/Calls.json")
}

/// A Twilio mock that answers updates with 200 and creates with 201 and the agent's sid.
async fn twilio_ok_mock() -> Mock {
    Mock::start(|r| {
        if r.path.ends_with("/Calls.json") {
            status(201, json!({ "sid": AGENT_SID, "status": "queued" }))
        } else {
            ok(json!({ "sid": CALLER_SID, "status": "in-progress" }))
        }
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Twilio
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn twilio_requests_use_basic_auth_and_form_encoding() {
    let mock = twilio_ok_mock().await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap())
    });
    control
        .transfer_cold(&caller(), &request(TransferMode::Cold, None))
        .await
        .unwrap();

    let sent = mock.requests();
    assert_eq!(sent.len(), 1);
    let r = &sent[0];
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("POST", update_path(CALLER_SID).as_str())
    );
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{ACCOUNT}:{TOKEN}"))
    );
    assert_eq!(r.header("authorization"), Some(expected.as_str()));
    assert_eq!(
        r.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(r.form().len(), 1);
    assert_eq!(
        r.form_get("Twiml").unwrap(),
        "<Response><Dial callerId=\"+908501234567\" answerOnBridge=\"true\" timeout=\"30\">\
         +905551112233</Dial></Response>"
    );
}

#[tokio::test]
async fn twilio_api_key_auth_uses_the_key_sid_as_user() {
    let mock = twilio_ok_mock().await;
    let control = twilio(&mock, |c| {
        c.auth = TwilioAuth::ApiKey {
            sid: "SK0123".to_owned(),
            secret: Secret::new(KEY.to_owned()),
        };
        c.dial_action_url = Some("https://app.example/dial?a=1&b=2".to_owned());
    });
    control
        .transfer_cold(&caller(), &request(TransferMode::Cold, None))
        .await
        .unwrap();
    let r = &mock.requests()[0];
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("SK0123:{KEY}"))
    );
    assert_eq!(r.header("authorization"), Some(expected.as_str()));
    // Path still uses the account SID; the action URL is XML-escaped inside the TwiML.
    assert_eq!(r.path, update_path(CALLER_SID));
    assert_eq!(
        r.form_get("Twiml").unwrap(),
        "<Response><Dial answerOnBridge=\"true\" timeout=\"30\" \
         action=\"https://app.example/dial?a=1&amp;b=2\">+905551112233</Dial></Response>"
    );
}

#[tokio::test]
async fn twilio_dtmf_needs_a_resume_stream_and_sends_play_digits_then_a_new_stream() {
    let mock = twilio_ok_mock().await;
    let digits = DtmfString::parse("1ww2#").unwrap();

    let plain = twilio(&mock, |_| {});
    assert!(!plain.capabilities().send_dtmf);
    let error = plain.send_dtmf(&caller(), &digits, 1).await.unwrap_err();
    assert!(matches!(error, Error::Unsupported { .. }), "{error}");
    assert!(!error.outcome_unknown());
    assert!(
        mock.requests().is_empty(),
        "nothing is sent when the binding cannot do it"
    );

    // A resume stream alone is not enough: the REST update ends the old stream, and the shipped
    // media adapter would handle that as a hang-up. The application must say its adapter copes.
    let unsure = twilio(&mock, |c| c.resume_stream = Some(resume()));
    assert!(!unsure.capabilities().send_dtmf);
    let error = unsure.send_dtmf(&caller(), &digits, 1).await.unwrap_err();
    assert!(matches!(error, Error::Unsupported { .. }), "{error}");
    assert!(mock.requests().is_empty());

    let control = twilio(&mock, |c| {
        c.resume_stream = Some(resume());
        c.adapter_resumes_session = true;
    });
    assert!(control.capabilities().send_dtmf);
    control.send_dtmf(&caller(), &digits, 1).await.unwrap();
    let r = &mock.requests()[0];
    assert_eq!(r.path, update_path(CALLER_SID));
    assert_eq!(
        r.form_get("Twiml").unwrap(),
        "<Response><Play digits=\"1ww2#\"/><Connect>\
         <Stream url=\"wss://agent.example.com/twilio/stream\" name=\"brisvox\">\
         <Parameter name=\"resume\" value=\"dtmf\"/>\
         <Parameter name=\"agent\" value=\"support-tr\"/></Stream></Connect></Response>"
    );
}

#[tokio::test]
async fn twilio_resumed_streams_get_a_url_made_for_their_call() {
    use brisvox_transport::call_control::twilio::StreamUrlFn;

    let mock = twilio_ok_mock().await;
    let digits = DtmfString::parse("5").unwrap();
    let control = twilio(&mock, |c| {
        c.resume_stream = Some(ResumeStream {
            url_for_call: Some(StreamUrlFn::new(|call| {
                Some(format!(
                    "wss://agent.example.com/twilio/stream/{}.9.mac",
                    call.as_str()
                ))
            })),
            ..resume()
        });
        c.adapter_resumes_session = true;
    });
    control.send_dtmf(&caller(), &digits, 1).await.unwrap();
    let twiml = mock.requests()[0].form_get("Twiml").unwrap();
    assert!(
        twiml.contains(&format!(
            "<Stream url=\"wss://agent.example.com/twilio/stream/{CALLER_SID}.9.mac\""
        )),
        "{twiml}"
    );

    // A URL that cannot be made fails the update before anything is sent.
    let before = mock.requests().len();
    let broken = twilio(&mock, |c| {
        c.resume_stream = Some(ResumeStream {
            url_for_call: Some(StreamUrlFn::new(|_| None)),
            ..resume()
        });
        c.adapter_resumes_session = true;
    });
    assert!(broken.send_dtmf(&caller(), &digits, 1).await.is_err());
    assert_eq!(mock.requests().len(), before);
}

#[tokio::test]
async fn twilio_announces_the_end_of_the_stream_before_dtmf_and_before_a_warm_transfer() {
    // The media adapter must know the stream's end is on purpose before the update that
    // causes it is sent.
    let mock = twilio_ok_mock().await;
    let log = Arc::clone(&mock.requests);
    let hooked: Arc<Mutex<Vec<(String, Duration, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&hooked);
    let control = twilio(&mock, move |c| {
        c.resume_stream = Some(resume());
        c.adapter_resumes_session = true;
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.agent_accept_window_secs = 100;
        c.dial_timeout_secs = 20;
        c.on_stream_restart = Some(StreamRestartHook::new(move |call, wait| {
            sink.lock()
                .unwrap()
                .push((call.as_str().to_owned(), wait, log.lock().unwrap().len()));
        }));
    });
    control
        .send_dtmf(&caller(), &DtmfString::parse("12").unwrap(), 1)
        .await
        .unwrap();
    control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    // A cold transfer ends the bot for good: not announced.
    control
        .transfer_cold(&caller(), &request(TransferMode::Cold, None))
        .await
        .unwrap();
    // Each announcement came before its update: none had been sent for the DTMF, one for the
    // warm transfer's first step.
    assert_eq!(
        *hooked.lock().unwrap(),
        [
            (CALLER_SID.to_owned(), Duration::from_secs(30), 0),
            (CALLER_SID.to_owned(), Duration::from_secs(180), 1),
        ]
    );
}

/// A control whose restart hook logs `announce` / `retract` calls for the caller's sid.
fn twilio_with_logging_hook(mock: &Mock, log: &Arc<Mutex<Vec<&'static str>>>) -> TwilioCallControl {
    let announced = Arc::clone(log);
    let retracted = Arc::clone(log);
    twilio(mock, move |c| {
        c.resume_stream = Some(resume());
        c.adapter_resumes_session = true;
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.on_stream_restart = Some(
            StreamRestartHook::new(move |_, _| announced.lock().unwrap().push("announce"))
                .with_retraction(move |call| {
                    assert_eq!(call.as_str(), CALLER_SID);
                    retracted.lock().unwrap().push("retract");
                }),
        );
    })
}

#[tokio::test]
async fn twilio_retracts_the_announcement_when_the_update_is_refused_for_certain() {
    // A definitely refused update leaves the stream up, so the announcement of its
    // end must not stay armed (a hang-up within its window would be taken for the restart).
    let mock = Mock::start(|_| status(400, json!({ "code": 21220, "message": "no" }))).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let control = twilio_with_logging_hook(&mock, &log);
    let error = control
        .send_dtmf(&caller(), &DtmfString::parse("12").unwrap(), 1)
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 400, .. }),
        "{error}"
    );
    assert_eq!(*log.lock().unwrap(), ["announce", "retract"]);

    log.lock().unwrap().clear();
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 400, .. }),
        "{error}"
    );
    assert_eq!(
        *log.lock().unwrap(),
        ["announce", "retract"],
        "warm: step 1 refused"
    );
}

#[tokio::test]
async fn twilio_keeps_the_announcement_when_the_outcome_of_the_update_is_unknown() {
    // A 5xx may have been applied: the stream may still end, so the announcement stays.
    let mock = Mock::start(|_| status(503, json!({}))).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let control = twilio_with_logging_hook(&mock, &log);
    control
        .send_dtmf(&caller(), &DtmfString::parse("12").unwrap(), 1)
        .await
        .unwrap_err();
    assert_eq!(*log.lock().unwrap(), ["announce"]);
}

#[tokio::test]
async fn twilio_hangup_completes_the_call() {
    let mock = twilio_ok_mock().await;
    twilio(&mock, |_| {}).hangup(&caller(), 7).await.unwrap();
    let r = &mock.requests()[0];
    assert_eq!(r.path, update_path(CALLER_SID));
    assert_eq!(
        r.form(),
        vec![("Status".to_owned(), "completed".to_owned())]
    );
}

#[tokio::test]
async fn twilio_warm_transfer_parks_the_caller_then_calls_the_agent_with_a_briefing() {
    let mock = twilio_ok_mock().await;
    let mut amd = brisvox_transport::call_control::AmdConfig::new(
        brisvox_transport::call_control::AmdMode::DetectMessageEnd,
    );
    amd.timeout = Some(Duration::from_secs(20));
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.whisper_language = "tr-TR".to_owned();
        c.status_callback = Some("https://app.example/status".to_owned());
        c.conference_wait_url = Some("https://app.example/hold".to_owned());
        c.agent_leg_amd = Some(amd);
    });
    assert!(control.capabilities().warm_transfer);

    let transfer = control
        .transfer_warm(
            &caller(),
            &request(TransferMode::Warm, Some("Ali & Veli want a <refund>")),
        )
        .await
        .unwrap();
    assert_eq!(transfer.provider, "twilio");
    assert_eq!(transfer.caller, caller());
    assert_eq!(
        transfer.agent_leg.as_ref().map(CallRef::as_str),
        Some(AGENT_SID)
    );
    assert_eq!(transfer.conference, "brisvox-0000000000000abc");
    assert_eq!(transfer.briefing, Briefing::Whisper);
    assert!(
        !format!("{transfer:?}").contains("refund"),
        "the summary stays out of Debug output"
    );

    let sent = mock.requests();
    assert_eq!(sent.len(), 2);
    // 1: the caller waits in the conference.
    assert_eq!(sent[0].path, update_path(CALLER_SID));
    assert_eq!(
        sent[0].form_get("Twiml").unwrap(),
        "<Response><Dial><Conference beep=\"false\" startConferenceOnEnter=\"false\" \
         endConferenceOnExit=\"true\" participantLabel=\"caller\" \
         waitUrl=\"https://app.example/hold\">brisvox-0000000000000abc</Conference></Dial></Response>"
    );
    // 2: the agent leg only waits, silently: it is NOT in the conference and nothing is read to
    // it (a voicemail on the agent's phone must neither join nor hear the summary).
    assert_eq!(sent[1].path, create_path());
    assert_eq!(sent[1].form_get("To").as_deref(), Some("+905551112233"));
    assert_eq!(sent[1].form_get("From").as_deref(), Some("+908501234567"));
    assert_eq!(sent[1].form_get("Timeout").as_deref(), Some("30"));
    assert_eq!(
        sent[1].form_get("Twiml").unwrap(),
        "<Response><Pause length=\"60\"/><Pause length=\"60\"/></Response>"
    );
    assert_eq!(
        sent[1].form_get("StatusCallback").as_deref(),
        Some("https://app.example/status")
    );
    assert_eq!(
        sent[1].form_all("StatusCallbackEvent"),
        ["initiated", "ringing", "answered", "completed"]
    );
    assert_eq!(
        sent[1].form_get("MachineDetection").as_deref(),
        Some("DetectMessageEnd")
    );
    assert_eq!(
        sent[1].form_get("MachineDetectionTimeout").as_deref(),
        Some("20")
    );

    // Nothing joins the agent until the application accepts the answer.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(mock.requests().len(), 2, "no join without complete_warm");

    // complete_warm is the accept gate: the agent leg reads the summary, then joins.
    control.complete_warm(&transfer).await.unwrap();
    let sent = mock.requests();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2].path, update_path(AGENT_SID));
    assert_eq!(
        sent[2].form_get("Twiml").unwrap(),
        "<Response><Say language=\"tr-TR\">Ali &amp; Veli want a &lt;refund&gt;</Say><Dial>\
         <Conference beep=\"false\" startConferenceOnEnter=\"true\" endConferenceOnExit=\"false\" \
         participantLabel=\"agent\">brisvox-0000000000000abc</Conference></Dial></Response>"
    );
}

#[tokio::test]
async fn twilio_warm_transfer_without_a_summary_has_no_say() {
    let mock = twilio_ok_mock().await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    assert_eq!(transfer.briefing, Briefing::None);
    let hold = mock.requests()[1].form_get("Twiml").unwrap();
    assert!(!hold.contains("Conference"), "{hold}");
    control.complete_warm(&transfer).await.unwrap();
    let twiml = mock.requests()[2].form_get("Twiml").unwrap();
    assert!(twiml.starts_with("<Response><Dial><Conference"), "{twiml}");
}

#[tokio::test]
async fn twilio_complete_warm_needs_an_agent_leg_and_a_long_hold_is_split_into_pauses() {
    let mock = twilio_ok_mock().await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.agent_accept_window_secs = 150;
    });
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    assert_eq!(
        mock.requests()[1].form_get("Twiml").unwrap(),
        "<Response><Pause length=\"60\"/><Pause length=\"60\"/><Pause length=\"30\"/></Response>"
    );
    let mut handle = transfer.clone();
    handle.agent_leg = None;
    let error = control.complete_warm(&handle).await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(mock.requests().len(), 2);

    let mut config = twilio_config(&mock);
    config.agent_accept_window_secs = 1;
    assert!(TwilioCallControl::new(config, clock()).is_err());
}

#[tokio::test]
async fn twilio_failed_agent_call_puts_the_caller_back_on_the_bot() {
    let mock = Mock::start(|r| {
        if r.path.ends_with("/Calls.json") {
            status(
                400,
                json!({ "code": 21211, "message": "Invalid 'To' Phone Number", "status": 400 }),
            )
        } else {
            ok(json!({ "sid": CALLER_SID, "status": "in-progress" }))
        }
    })
    .await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.resume_stream = Some(resume());
    });
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    match &error {
        Error::WarmFailed {
            cause,
            caller,
            transfer,
        } => {
            assert_eq!(*caller, CallerState::Resumed);
            assert!(transfer.is_none(), "nothing is left to recover");
            assert!(
                matches!(**cause, Error::Rejected { status: 400, .. }),
                "{cause}"
            );
        }
        other => panic!("{other}"),
    }
    // A clean rollback is a definite failure: the model is told nothing was done and may try
    // another destination (the bound-call test below runs it through the tool).
    assert_eq!(error.kind(), TelephonyErrorKind::Rejected);
    let sent = mock.requests();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2].path, update_path(CALLER_SID));
    let twiml = sent[2].form_get("Twiml").unwrap();
    assert!(
        twiml.contains("<Connect><Stream url=\"wss://agent.example.com/twilio/stream\""),
        "{twiml}"
    );
    assert!(
        twiml.contains("<Parameter name=\"resume\" value=\"warm_failed\"/>"),
        "{twiml}"
    );
    let text = error.to_string();
    assert!(
        text.contains("code 21211") && !text.contains(TOKEN),
        "{text}"
    );
}

#[tokio::test]
async fn twilio_failed_agent_call_without_a_resume_stream_leaves_the_caller_parked() {
    let mock = Mock::start(|r| {
        if r.path.ends_with("/Calls.json") {
            status(500, json!({}))
        } else {
            ok(json!({ "sid": CALLER_SID }))
        }
    })
    .await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::WarmFailed {
                caller: CallerState::Parked,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.outcome_unknown(), "a 500 may have created the call");
    assert_eq!(
        mock.requests().len(),
        2,
        "no resume request without a resume stream"
    );
    // The caller waits in the conference: the error carries the recovery handle.
    match error {
        Error::WarmFailed { transfer, .. } => {
            let handle = transfer.expect("a parked caller comes with a recovery handle");
            assert_eq!(handle.agent_leg, None);
            assert_eq!(handle.caller, caller());
            assert_eq!(handle.conference, "brisvox-0000000000000abc");
            // Without a resume stream the abort cannot bring the caller back, and says so.
            let state = control.abort_warm(&handle).await.unwrap();
            assert_eq!(state, CallerState::Parked);
            assert_eq!(mock.requests().len(), 2, "no agent leg to end, no resume");
        }
        other => panic!("{other}"),
    }
}

#[tokio::test]
async fn twilio_agent_call_with_an_unknown_outcome_does_not_resume_the_caller_behind_it() {
    // The create request times out: Twilio may have created the leg. Resuming the caller on the
    // bot behind it would orphan a live leg, so the caller stays parked with a recovery handle.
    let mock = Mock::start(|r| {
        if r.path.ends_with("/Calls.json") {
            status(201, json!({ "sid": AGENT_SID })).after(Duration::from_secs(2))
        } else {
            ok(json!({ "sid": CALLER_SID }))
        }
    })
    .await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.resume_stream = Some(resume());
        c.request_timeout = Duration::from_millis(200);
    });
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    let handle = match error {
        Error::WarmFailed {
            cause,
            caller,
            transfer,
        } => {
            assert!(matches!(*cause, Error::Timeout { .. }), "{cause}");
            assert_eq!(caller, CallerState::Parked);
            transfer.expect("recovery handle")
        }
        other => panic!("{other}"),
    };
    assert_eq!(mock.requests().len(), 2, "the caller was not resumed");
    // The application decides with the handle: abort resumes the caller (no leg id to end).
    let state = control.abort_warm(&handle).await.unwrap();
    assert_eq!(state, CallerState::Resumed);
    let sent = mock.requests();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2].path, update_path(CALLER_SID));
}

#[tokio::test]
async fn twilio_park_failure_with_an_unknown_outcome_hands_back_a_recovery_handle() {
    let mock = Mock::start(|_| status(502, json!({}))).await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::WarmFailed {
                caller: CallerState::Parked,
                transfer: Some(_),
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(mock.requests().len(), 1, "the agent was not called");

    // A definite refusal changed nothing and is a plain error.
    let mock = Mock::start(|_| status(400, json!({ "code": 21205 }))).await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 400, .. }),
        "{error}"
    );
}

#[tokio::test]
async fn twilio_warm_transfer_validates_before_it_touches_the_call() {
    let mock = twilio_ok_mock().await;

    let no_caller_id = twilio(&mock, |_| {});
    assert!(!no_caller_id.capabilities().warm_transfer);
    let error = no_caller_id
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported { .. }), "{error}");

    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let long = "x".repeat(5000);
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, Some(&long)))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert!(mock.requests().is_empty(), "the caller was not touched");
}

#[tokio::test]
async fn a_warm_transfer_finishes_even_if_the_caller_drops_the_future() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let mock = Mock::start(move |r| {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        let answer = if r.path.ends_with("/Calls.json") {
            status(201, json!({ "sid": AGENT_SID }))
        } else {
            ok(json!({ "sid": CALLER_SID }))
        };
        if first {
            answer.after(Duration::from_millis(250))
        } else {
            answer
        }
    })
    .await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let request = request(TransferMode::Warm, None);
    let call = caller();
    let future = control.transfer_warm(&call, &request);
    let cancelled = tokio::time::timeout(Duration::from_millis(60), future).await;
    assert!(cancelled.is_err(), "the future was dropped mid-transfer");
    mock.wait_for(2, Duration::from_secs(3)).await;
    assert_eq!(mock.requests()[1].path, create_path(), "step 2 still ran");
}

#[tokio::test]
async fn twilio_abort_ends_the_agent_leg_and_resumes_the_caller() {
    let mock = twilio_ok_mock().await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.resume_stream = Some(resume());
    });
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    let state = control.abort_warm(&transfer).await.unwrap();
    assert_eq!(state, CallerState::Resumed);
    let sent = mock.requests();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[2].path, update_path(AGENT_SID));
    assert_eq!(sent[2].form_get("Status").as_deref(), Some("completed"));
    assert_eq!(sent[3].path, update_path(CALLER_SID));
    assert!(
        sent[3]
            .form_get("Twiml")
            .unwrap()
            .contains("value=\"warm_failed\"")
    );
}

#[tokio::test]
async fn twilio_abort_stops_at_the_first_error_so_a_retry_is_safe() {
    // Ending the agent leg fails: the caller must not be resumed yet.
    let failing = Arc::new(AtomicUsize::new(1));
    let flag = Arc::clone(&failing);
    let mock = Mock::start(move |r| {
        if r.path.ends_with("/Calls.json") {
            status(201, json!({ "sid": AGENT_SID }))
        } else if r.path.contains(AGENT_SID) && flag.load(Ordering::SeqCst) == 1 {
            status(503, json!({}))
        } else {
            ok(json!({ "sid": CALLER_SID }))
        }
    })
    .await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.resume_stream = Some(resume());
    });
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    let error = control.abort_warm(&transfer).await.unwrap_err();
    assert!(error.outcome_unknown(), "{error}");
    assert_eq!(mock.requests().len(), 3, "the caller was not resumed");
    failing.store(0, Ordering::SeqCst);
    assert_eq!(
        control.abort_warm(&transfer).await.unwrap(),
        CallerState::Resumed
    );
    assert_eq!(mock.requests().len(), 5);

    // An agent leg that is already gone is the goal reached.
    let mock = Mock::start(|r| {
        if r.path.ends_with("/Calls.json") {
            status(201, json!({ "sid": AGENT_SID }))
        } else if r.path.contains(AGENT_SID) {
            status(404, json!({ "code": 20404 }))
        } else {
            ok(json!({ "sid": CALLER_SID }))
        }
    })
    .await;
    let control = twilio(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.resume_stream = Some(resume());
    });
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    assert_eq!(
        control.abort_warm(&transfer).await.unwrap(),
        CallerState::Resumed
    );
}

#[tokio::test]
async fn twilio_errors_are_mapped_without_leaking_credentials() {
    let mock = Mock::start(|r| match r.path.as_str() {
        p if p.contains("CA00000000000000000000000000000404") => {
            status(404, json!({ "code": 20404 }))
        }
        p if p.contains("CA00000000000000000000000000000401") => status(
            401,
            json!({ "code": 20003, "message": format!("Authenticate with {TOKEN}") }),
        ),
        p if p.contains("CA00000000000000000000000000000500") => status(500, json!("oops")),
        p if p.contains("CA00000000000000000000000000000429") => status(
            429,
            json!({ "error_code": "20429", "error_message": "Too many requests\r\nnow" }),
        ),
        _ => ok(json!({})),
    })
    .await;
    let control = twilio(&mock, |_| {});
    let call = |tail: &str| CallRef::new(format!("CA00000000000000000000000000000{tail}")).unwrap();

    let gone = control.hangup(&call("404"), 1).await.unwrap_err();
    assert!(matches!(gone, Error::CallGone { .. }) && !gone.outcome_unknown());

    let unauthorized = control.hangup(&call("401"), 1).await.unwrap_err();
    match &unauthorized {
        Error::Rejected {
            status,
            code,
            detail,
            ..
        } => {
            assert_eq!(
                (*status, code.as_deref(), detail.as_str()),
                (401, Some("20003"), "")
            );
        }
        other => panic!("{other}"),
    }
    assert!(!format!("{unauthorized} {unauthorized:?}").contains(TOKEN));

    let server = control.hangup(&call("500"), 1).await.unwrap_err();
    assert!(server.outcome_unknown(), "a 5xx may have been applied");
    assert_eq!(
        brisvox_agent::TelephonyError::from(server).kind(),
        brisvox_agent::TelephonyErrorKind::OutcomeUnknown
    );

    let limited = control.hangup(&call("429"), 1).await.unwrap_err();
    assert!(!limited.outcome_unknown());
    match limited {
        Error::Rejected {
            status,
            code,
            detail,
            ..
        } => {
            assert_eq!((status, code.as_deref()), (429, Some("20429")));
            assert_eq!(
                detail, "Too many requests  now",
                "control characters are stripped"
            );
        }
        other => panic!("{other}"),
    }
}

#[tokio::test]
async fn a_slow_provider_times_out_and_the_outcome_is_unknown() {
    let mock = Mock::start(|_| ok(json!({})).after(Duration::from_secs(2))).await;
    let control = twilio(&mock, |c| c.request_timeout = Duration::from_millis(150));
    let start = Instant::now();
    let error = control.hangup(&caller(), 1).await.unwrap_err();
    assert!(matches!(error, Error::Timeout { .. }), "{error}");
    assert!(error.outcome_unknown());
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn an_unreachable_provider_means_nothing_was_applied() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut config = TwilioConfig::new(
        ACCOUNT,
        TwilioAuth::AuthToken(Secret::new(TOKEN.to_owned())),
    );
    config.base_url = format!("http://{addr}/2010-04-01");
    // Windows retries a refused connection for about two seconds before it reports it.
    config.request_timeout = Duration::from_secs(8);
    config.connect_timeout = Duration::from_secs(6);
    let control = TwilioCallControl::new(config, clock()).unwrap();
    let error = control.hangup(&caller(), 1).await.unwrap_err();
    assert!(matches!(error, Error::Unreachable { .. }), "{error}");
    assert!(!error.outcome_unknown());
    // reqwest's Display appends " for url (...)": the account and call ids must not get out.
    assert_no_url(&error);
    let telephony = brisvox_agent::TelephonyError::from(error);
    assert_eq!(
        telephony.kind(),
        brisvox_agent::TelephonyErrorKind::Unavailable
    );
    assert_no_url(&telephony);
}

fn assert_no_url(error: &(impl std::fmt::Display + std::fmt::Debug)) {
    let text = format!("{error} {error:?}");
    assert!(
        !text.contains(ACCOUNT) && !text.contains("/Calls") && !text.contains(CALLER_SID),
        "the request URL leaked into: {text}"
    );
    assert!(!text.contains("http://"), "{text}");
}

/// What a raw server does with a connection after it read the request.
#[derive(Clone, Copy)]
enum Raw {
    /// Close at once, no answer (a connection reset mid-request).
    Reset,
    /// Send the headers and a part of the body, then stall.
    Stall,
    /// Stream a chunked body that never ends.
    Endless,
}

/// A server that reads one request and then misbehaves. Returns its address and a counter of
/// body bytes it managed to write.
async fn raw_server(behavior: Raw) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let written = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&written);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(raw_connection(stream, behavior, Arc::clone(&counter)));
        }
    });
    (addr, written)
}

async fn raw_connection(mut stream: TcpStream, behavior: Raw, written: Arc<AtomicUsize>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= pos + 4 + length {
                break;
            }
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    match behavior {
        Raw::Reset => {}
        Raw::Stall => {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{\"status\":",
                )
                .await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        Raw::Endless => {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n",
                )
                .await;
            let block = vec![b'x'; 8192];
            let mut frame = format!("{:x}\r\n", block.len()).into_bytes();
            frame.extend_from_slice(&block);
            frame.extend_from_slice(b"\r\n");
            // Bounded so a broken client cannot keep this task alive for ever.
            for _ in 0..4096 {
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                written.fetch_add(block.len(), Ordering::SeqCst);
            }
        }
    }
}

fn twilio_at(addr: SocketAddr, timeout: Duration) -> TwilioCallControl {
    let mut config = TwilioConfig::new(
        ACCOUNT,
        TwilioAuth::AuthToken(Secret::new(TOKEN.to_owned())),
    );
    config.base_url = format!("http://{addr}{TWILIO_PREFIX}");
    config.request_timeout = timeout;
    TwilioCallControl::new(config, clock()).unwrap()
}

#[tokio::test]
async fn a_connection_that_drops_mid_request_is_an_unknown_outcome_without_the_url() {
    let (addr, _) = raw_server(Raw::Reset).await;
    let control = twilio_at(addr, Duration::from_secs(3));
    let error = control.hangup(&caller(), 1).await.unwrap_err();
    assert!(matches!(error, Error::Transport { .. }), "{error}");
    assert!(error.outcome_unknown(), "the request had left");
    assert_no_url(&error);
}

#[tokio::test]
async fn the_deadline_covers_the_response_body_not_just_the_headers() {
    // Headers and a bit of body arrive, then the server stalls: the call must still end at the
    // request timeout (a regression that timed only `send()` would hang the tool task until the
    // engine's 120 s backstop).
    let (addr, _) = raw_server(Raw::Stall).await;
    let control = twilio_at(addr, Duration::from_millis(300));
    let start = Instant::now();
    let error = control.call_status(&caller()).await.unwrap_err();
    assert!(matches!(error, Error::Timeout { .. }), "{error}");
    assert!(error.outcome_unknown());
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn an_endless_body_is_cut_at_the_cap_and_returns_within_the_deadline() {
    let (addr, written) = raw_server(Raw::Endless).await;
    let control = twilio_at(addr, Duration::from_secs(3));
    let start = Instant::now();
    // A 2xx whose body nobody reads: the reply is complete once the cap is reached.
    control.hangup(&caller(), 1).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    // The cap is 64 KiB: the client stops reading long before the server's 32 MiB.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        written.load(Ordering::SeqCst) < 8 * 1024 * 1024,
        "{} bytes were written",
        written.load(Ordering::SeqCst)
    );
    // A body that is cut cannot be parsed: a read that needs it fails cleanly instead of hanging.
    let error = control.call_status(&caller()).await.unwrap_err();
    assert!(matches!(error, Error::Protocol { .. }), "{error}");
}

#[tokio::test]
async fn twilio_call_status_reads_status_and_amd_verdict() {
    let mock =
        Mock::start(|_| ok(json!({ "status": "in-progress", "answered_by": "machine_end_beep" })))
            .await;
    let control = twilio(&mock, |_| {});
    let snapshot = control.call_status(&caller()).await.unwrap();
    assert_eq!(
        snapshot.status,
        brisvox_transport::call_control::twilio::TwilioCallStatus::InProgress
    );
    assert_eq!(
        snapshot.answered_by,
        Some(brisvox_agent::AnsweredBy::MachineEndBeep)
    );
    let r = &mock.requests()[0];
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("GET", update_path(CALLER_SID).as_str())
    );
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn configuration_is_checked_and_credentials_stay_out_of_debug_output() {
    let mock = twilio_ok_mock().await;

    let mut remote = twilio_config(&mock);
    remote.base_url = "http://api.twilio.com/2010-04-01".to_owned();
    assert!(matches!(
        TwilioCallControl::new(remote, clock()),
        Err(Error::Config(_))
    ));

    let mut bad_sid = twilio_config(&mock);
    bad_sid.account_sid = "AC/../x".to_owned();
    assert!(TwilioCallControl::new(bad_sid, clock()).is_err());

    let mut bad_timeout = twilio_config(&mock);
    bad_timeout.dial_timeout_secs = 1;
    assert!(TwilioCallControl::new(bad_timeout, clock()).is_err());

    let control = twilio(&mock, |_| {});
    let debug = format!("{control:?} {:?}", control.config());
    assert!(
        !debug.contains(TOKEN) && debug.contains("[REDACTED]"),
        "{debug}"
    );

    // A call id cannot alter the URL path.
    assert!(CallRef::new("CA1/../../x").is_err());
    assert!(CallRef::new("").is_err());
    assert!(CallRef::new("CA1?x=1").is_err());
}

// ---------------------------------------------------------------------------------------------
// Telnyx
// ---------------------------------------------------------------------------------------------

const TELNYX_KEY: &str = "KEY0123456789-telnyx-secret";
const CC_ID: &str = "v3:caller-call-control-id";
const AGENT_CC: &str = "v3:agent-call-control-id";

fn telnyx_config(mock: &Mock) -> TelnyxConfig {
    let mut config = TelnyxConfig::new(Secret::new(TELNYX_KEY.to_owned()));
    config.base_url = mock.url("/v2");
    config.request_timeout = Duration::from_secs(3);
    config
}

fn telnyx(mock: &Mock, tweak: impl FnOnce(&mut TelnyxConfig)) -> TelnyxCallControl {
    let mut config = telnyx_config(mock);
    tweak(&mut config);
    TelnyxCallControl::new(config, clock()).unwrap()
}

fn tcall() -> CallRef {
    CallRef::new(CC_ID).unwrap()
}

fn telnyx_ok() -> Answer {
    ok(json!({ "data": { "result": "ok" } }))
}

fn telnyx_error(code: &str) -> Answer {
    status(
        422,
        json!({ "errors": [{ "code": code, "title": "Something", "detail": "details here" }] }),
    )
}

#[tokio::test]
async fn telnyx_dtmf_is_a_json_command_with_a_bearer_key_and_a_command_id() {
    let mock = Mock::start(|_| telnyx_ok()).await;
    let control = telnyx(&mock, |_| {});
    assert!(control.capabilities().send_dtmf);
    control
        .send_dtmf(&tcall(), &DtmfString::parse("1www2#").unwrap(), 0xabc)
        .await
        .unwrap();
    let r = &mock.requests()[0];
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        (
            "POST",
            "/v2/calls/v3:caller-call-control-id/actions/send_dtmf"
        )
    );
    assert_eq!(
        r.header("authorization"),
        Some(format!("Bearer {TELNYX_KEY}").as_str())
    );
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(
        r.json(),
        json!({ "digits": "1www2#", "duration_millis": 250, "command_id": "brisvox-0000000000000abc" })
    );

    let mock = Mock::start(|_| telnyx_ok()).await;
    let control = telnyx(&mock, |c| c.dtmf_duration = Duration::from_millis(400));
    control
        .send_dtmf(&tcall(), &DtmfString::parse("5").unwrap(), 1)
        .await
        .unwrap();
    assert_eq!(mock.requests()[0].json()["duration_millis"], 400);
}

#[tokio::test]
async fn telnyx_cold_transfer_body() {
    let mock = Mock::start(|_| telnyx_ok()).await;
    let control = telnyx(&mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap())
    });
    control
        .transfer_cold(&tcall(), &request(TransferMode::Cold, None))
        .await
        .unwrap();
    let sent = mock.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].path,
        "/v2/calls/v3:caller-call-control-id/actions/transfer"
    );
    assert_eq!(
        sent[0].json(),
        json!({
            "to": "+905551112233",
            "from": "+908501234567",
            "timeout_secs": 30,
            "command_id": "brisvox-0000000000000abc",
        })
    );

    // `from` defaults to the original call's `to` when not configured.
    let mock = Mock::start(|_| telnyx_ok()).await;
    telnyx(&mock, |_| {})
        .transfer_cold(&tcall(), &request(TransferMode::Cold, None))
        .await
        .unwrap();
    assert!(mock.requests()[0].json().get("from").is_none());
}

#[tokio::test]
async fn telnyx_transfer_refused_while_streaming_stops_the_stream_and_retries_once() {
    let transfers = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&transfers);
    let mock = Mock::start(move |r| {
        if r.path.ends_with("/actions/transfer") && counter.fetch_add(1, Ordering::SeqCst) == 0 {
            telnyx_error("90045")
        } else {
            telnyx_ok()
        }
    })
    .await;
    let control = telnyx(&mock, |_| {});
    control
        .transfer_cold(&tcall(), &request(TransferMode::Cold, None))
        .await
        .unwrap();
    let sent = mock.requests();
    let steps: Vec<&str> = sent
        .iter()
        .map(|r| r.path.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(steps, ["transfer", "streaming_stop", "transfer"]);
    assert_eq!(sent[0].json()["command_id"], "brisvox-0000000000000abc");
    assert_eq!(
        sent[1].json()["command_id"],
        "brisvox-0000000000000abc.stop"
    );
    assert_eq!(sent[2].json()["command_id"], "brisvox-0000000000000abc.r1");
    assert_eq!(sent[2].json()["to"], "+905551112233");
}

#[tokio::test]
async fn telnyx_stream_conflicts_can_be_left_alone_and_dtmf_never_stops_the_stream() {
    let mock = Mock::start(|_| telnyx_error("90045")).await;
    let control = telnyx(&mock, |c| c.stop_stream_on_conflict = false);
    let error = control
        .transfer_cold(&tcall(), &request(TransferMode::Cold, None))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::StreamActive { .. }), "{error}");
    assert!(!error.outcome_unknown());
    assert_eq!(mock.requests().len(), 1);

    let mock = Mock::start(|_| telnyx_error("90045")).await;
    let control = telnyx(&mock, |_| {});
    let error = control
        .send_dtmf(&tcall(), &DtmfString::parse("1").unwrap(), 1)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::StreamActive { .. }), "{error}");
    assert_eq!(
        mock.requests().len(),
        1,
        "DTMF does not stop the bot's stream"
    );

    // A failing stop is reported as it is, and the transfer is not repeated.
    let mock = Mock::start(|r| {
        if r.path.ends_with("/actions/transfer") {
            telnyx_error("90045")
        } else {
            telnyx_error("90018")
        }
    })
    .await;
    let control = telnyx(&mock, |_| {});
    let error = control
        .transfer_cold(&tcall(), &request(TransferMode::Cold, None))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::CallGone { .. }), "{error}");
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn telnyx_warm_transfer_creates_a_conference_dials_the_agent_and_joins_it() {
    let mock = Mock::start(|r| match r.path.as_str() {
        "/v2/conferences" => ok(json!({ "data": { "id": "conf-123", "name": "x" } })),
        "/v2/calls" => ok(json!({ "data": { "call_control_id": AGENT_CC, "is_alive": false } })),
        _ => telnyx_ok(),
    })
    .await;
    let mut amd = brisvox_transport::call_control::AmdConfig::new(
        brisvox_transport::call_control::AmdMode::DetectMessageEnd,
    );
    amd.timeout = Some(Duration::from_secs(10));
    let control = telnyx(&mock, |c| {
        c.connection_id = Some("conn-42".to_owned());
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.hold_audio_url = Some("https://app.example/hold.mp3".to_owned());
        c.agent_leg_amd = Some(amd);
    });
    assert!(control.capabilities().warm_transfer);
    assert!(!control.capabilities().warm_briefing);

    let transfer = control
        .transfer_warm(
            &tcall(),
            &request(TransferMode::Warm, Some("refund for order 12")),
        )
        .await
        .unwrap();
    assert_eq!(transfer.provider, "telnyx");
    assert_eq!(transfer.conference, "conf-123");
    assert_eq!(
        transfer.agent_leg.as_ref().map(CallRef::as_str),
        Some(AGENT_CC)
    );
    assert_eq!(
        transfer.briefing,
        Briefing::NotSupported,
        "the summary cannot be read to the agent"
    );

    let sent = mock.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].json(),
        json!({
            "call_control_id": CC_ID,
            "name": "brisvox-0000000000000abc",
            "start_conference_on_create": true,
            "hold_audio_url": "https://app.example/hold.mp3",
        })
    );
    assert_eq!(sent[1].path, "/v2/calls");
    assert_eq!(
        sent[1].json(),
        json!({
            "connection_id": "conn-42",
            "to": "+905551112233",
            "from": "+908501234567",
            "link_to": CC_ID,
            "timeout_secs": 30,
            "command_id": "brisvox-0000000000000abc.dial",
            "answering_machine_detection": "greeting_end",
            "answering_machine_detection_config": {
                "total_analysis_time_millis": 10000,
                "greeting_total_analysis_time_millis": 10000,
            },
        })
    );

    control.complete_warm(&transfer).await.unwrap();
    let join = &mock.requests()[2];
    assert_eq!(join.path, "/v2/conferences/conf-123/actions/join");
    assert_eq!(
        join.json(),
        json!({
            "call_control_id": AGENT_CC,
            "start_conference_on_enter": true,
            "end_conference_on_exit": false,
        })
    );

    // The agent leg is ended; the caller cannot go back to the bot on Telnyx and the state says so.
    let state = control.abort_warm(&transfer).await.unwrap();
    assert_eq!(state, CallerState::Parked);
    let hangup = &mock.requests()[3];
    assert_eq!(
        hangup.path,
        "/v2/calls/v3:agent-call-control-id/actions/hangup"
    );
}

#[tokio::test]
async fn telnyx_warm_transfer_failures() {
    // Not configured: nothing is sent.
    let mock = Mock::start(|_| telnyx_ok()).await;
    let control = telnyx(&mock, |_| {});
    assert!(!control.capabilities().warm_transfer);
    let error = control
        .transfer_warm(&tcall(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported { .. }), "{error}");
    assert!(mock.requests().is_empty());

    // The conference response has no id: stop before dialing (the shape is UNVERIFIED).
    let mock = Mock::start(|_| ok(json!({ "data": {} }))).await;
    let control = telnyx(&mock, |c| {
        c.connection_id = Some("conn-42".to_owned());
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let error = control
        .transfer_warm(&tcall(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Protocol { .. }) && error.outcome_unknown(),
        "{error}"
    );
    assert_eq!(mock.requests().len(), 1);

    // The dial fails: the caller is left parked in the conference, with a recovery handle.
    let mock = Mock::start(|r| {
        if r.path == "/v2/calls" {
            telnyx_error("10007")
        } else {
            ok(json!({ "data": { "id": "conf-9" } }))
        }
    })
    .await;
    let control = telnyx(&mock, |c| {
        c.connection_id = Some("conn-42".to_owned());
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let error = control
        .transfer_warm(&tcall(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::WarmFailed {
                caller: CallerState::Parked,
                ..
            }
        ),
        "{error}"
    );
    let Error::WarmFailed { transfer, .. } = error else {
        panic!("not a warm failure");
    };
    let handle = transfer.expect("the conference id is handed back");
    assert_eq!(
        (handle.conference.as_str(), handle.agent_leg.clone()),
        ("conf-9", None)
    );
    let before = mock.requests().len();
    // Aborting a handle without an agent leg ends nothing, and reports the caller as parked.
    assert_eq!(
        control.abort_warm(&handle).await.unwrap(),
        CallerState::Parked
    );
    assert_eq!(mock.requests().len(), before);
    let error = control.complete_warm(&handle).await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");

    // The dial is accepted but its answer has no leg id: same recovery handle.
    let mock = Mock::start(|r| {
        if r.path == "/v2/calls" {
            ok(json!({ "data": {} }))
        } else {
            ok(json!({ "data": { "id": "conf-8" } }))
        }
    })
    .await;
    let control = telnyx(&mock, |c| {
        c.connection_id = Some("conn-42".to_owned());
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    });
    let error = control
        .transfer_warm(&tcall(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::WarmFailed {
                caller: CallerState::Parked,
                transfer: Some(_),
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn only_the_message_end_modes_report_the_end_of_a_greeting() {
    use brisvox_transport::call_control::{AmdConfig, AmdMode};
    let detect = AmdConfig::new(AmdMode::Detect);
    let end = AmdConfig::new(AmdMode::DetectMessageEnd);
    assert!(!detect.twilio_provides_greeting_end() && !detect.telnyx_provides_greeting_end());
    assert!(end.twilio_provides_greeting_end() && end.telnyx_provides_greeting_end());
    let mut premium = AmdConfig::new(AmdMode::Detect);
    premium.premium = true;
    assert!(premium.telnyx_provides_greeting_end() && !premium.twilio_provides_greeting_end());
}

#[test]
fn telnyx_detect_mode_ends_at_machine_start_so_leave_message_needs_a_detect_only_policy() {
    use brisvox_agent::telephony::voicemail::{
        CallOutcome, Step, VoicemailDriver, VoicemailNode, VoicemailPolicy,
    };
    use brisvox_transport::call_control::parse_telnyx_amd_event;
    // Telnyx `detect` sends only call.machine.detection.ended: no greeting event follows.
    let body = json!({ "data": {
        "event_type": "call.machine.detection.ended",
        "id": "evt-1",
        "payload": { "call_control_id": AGENT_CC, "result": "machine" }
    }})
    .to_string();
    let event = parse_telnyx_amd_event(body.as_bytes()).unwrap().unwrap();
    assert_eq!(event.answered_by, brisvox_agent::AnsweredBy::MachineStart);

    #[allow(clippy::disallowed_methods)]
    let t0 = tokio::time::Instant::now();
    // The default policy would now wait 30 s for a greeting end that never arrives ...
    let mut waiting = VoicemailDriver::new(VoicemailPolicy::default(), t0);
    assert_eq!(
        waiting.on_verdict(event.answered_by, t0),
        Step::Enter(VoicemailNode::WaitGreeting)
    );
    assert_eq!(waiting.deadline(), Some(t0 + Duration::from_secs(30)));
    // ... the detect-only policy ends the call on the machine instead.
    let mut driver = VoicemailDriver::new(VoicemailPolicy::detect_only(), t0);
    assert_eq!(driver.on_verdict(event.answered_by, t0), Step::HangUp);
    assert_eq!(driver.outcome(), Some(CallOutcome::MachineHangUp));
}

#[tokio::test]
async fn telnyx_errors_hangup_and_secrecy() {
    let mock = Mock::start(|r| match r.path.as_str() {
        p if p.contains("gone") => telnyx_error("90018"),
        p if p.contains("denied") => status(
            403,
            json!({ "errors": [{ "code": "10010", "detail": format!("bad key {TELNYX_KEY}") }] }),
        ),
        p if p.contains("limited") => status(
            429,
            json!({ "errors": [{ "code": 10006, "detail": "slow down" }] }),
        ),
        _ => telnyx_ok(),
    })
    .await;
    let control = telnyx(&mock, |_| {});

    control.hangup(&tcall(), 0xabc).await.unwrap();
    assert_eq!(
        mock.requests()[0].path,
        "/v2/calls/v3:caller-call-control-id/actions/hangup"
    );
    assert_eq!(
        mock.requests()[0].json(),
        json!({ "command_id": "brisvox-0000000000000abc.hangup" })
    );

    let gone = control
        .hangup(&CallRef::new("v3:gone").unwrap(), 1)
        .await
        .unwrap_err();
    assert!(matches!(gone, Error::CallGone { .. }));

    let denied = control
        .hangup(&CallRef::new("v3:denied").unwrap(), 1)
        .await
        .unwrap_err();
    assert!(
        !format!("{denied} {denied:?}").contains(TELNYX_KEY),
        "the provider's text is dropped for 401/403"
    );

    let limited = control
        .hangup(&CallRef::new("v3:limited").unwrap(), 1)
        .await
        .unwrap_err();
    match limited {
        Error::Rejected {
            status,
            code,
            detail,
            ..
        } => {
            assert_eq!(
                (status, code.as_deref(), detail.as_str()),
                (429, Some("10006"), "slow down")
            );
        }
        other => panic!("{other}"),
    }

    let debug = format!("{control:?}");
    assert!(
        !debug.contains(TELNYX_KEY) && debug.contains("[REDACTED]"),
        "{debug}"
    );
}

#[tokio::test]
async fn telnyx_timeouts_and_config_checks() {
    let mock = Mock::start(|_| telnyx_ok().after(Duration::from_secs(2))).await;
    let control = telnyx(&mock, |c| c.request_timeout = Duration::from_millis(150));
    let error = control.hangup(&tcall(), 1).await.unwrap_err();
    assert!(
        matches!(error, Error::Timeout { .. }) && error.outcome_unknown(),
        "{error}"
    );

    let mut config = telnyx_config(&mock);
    config.dtmf_duration = Duration::from_millis(50);
    assert!(TelnyxCallControl::new(config, clock()).is_err());
    let mut config = telnyx_config(&mock);
    config.base_url = "http://api.telnyx.com/v2".to_owned();
    assert!(TelnyxCallControl::new(config, clock()).is_err());
    let mut config = telnyx_config(&mock);
    config.connection_id = Some("bad/id".to_owned());
    assert!(TelnyxCallControl::new(config, clock()).is_err());
}

// ---------------------------------------------------------------------------------------------
// The agent tools end to end over a bound call
// ---------------------------------------------------------------------------------------------

fn tool_call(turn: u32, name: &str, arguments: Value) -> Assembled {
    Assembled::Complete(ToolCall {
        id: ToolCallId::new(TurnEpoch::new(turn), 0),
        provider_id: None,
        name: Arc::from(name),
        arguments,
        provider_data: None,
    })
}

async fn run_tool(engine: &ToolEngine, turn: u32, name: &str, args: Value) -> (bool, Value) {
    let mut execution = engine
        .execute(
            vec![tool_call(turn, name, args)],
            TurnEpoch::new(turn),
            &CancellationToken::new(),
        )
        .await;
    // The result reaches the LLM as the runner would deliver it; an undelivered SideEffect call
    // would be an orphan and an identical later call would get its recorded result.
    execution.mark_delivered();
    match execution.results.first() {
        Some(Message::Tool {
            content, is_error, ..
        }) => (*is_error, serde_json::from_str(content).unwrap()),
        other => panic!("{other:?}"),
    }
}

fn tool_engine(bound: &Arc<BoundCall>, policy: &TelephonyPolicy) -> ToolEngine {
    let ops: Arc<dyn TelephonyOps> = bound.clone();
    let mut registry = ToolRegistry::new();
    register_telephony_tools(&mut registry, &ops, policy, &clock()).unwrap();
    ToolEngine::new(
        Arc::new(registry),
        SessionId::new(1),
        clock(),
        EngineConfig::default(),
    )
}

#[tokio::test]
async fn the_transfer_tool_dials_through_twilio_and_refuses_a_second_transfer() {
    let mock = twilio_ok_mock().await;
    let control = Arc::new(twilio(&mock, |_| {}));
    let bound = Arc::new(BoundCall::new(control, caller()));
    let policy = TelephonyPolicy::new()
        .destination("sales", "+905551112233", "the sales team")
        .unwrap();
    let engine = tool_engine(&bound, &policy);

    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(!is_error, "{body}");
    assert_eq!(body["status"], "initiated");
    let sent = mock.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].path, update_path(CALLER_SID));
    assert!(
        sent[0]
            .form_get("Twiml")
            .unwrap()
            .contains(">+905551112233</Dial>")
    );

    let (is_error, _) = run_tool(
        &engine,
        2,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(is_error);
    assert_eq!(mock.requests().len(), 1, "one transfer per call");

    // A number the model invents never reaches the provider.
    let (is_error, body) = run_tool(
        &engine,
        3,
        "transfer_call",
        json!({ "destination": "+9005551230000" }),
    )
    .await;
    assert!(is_error);
    assert_eq!(body["error"], "invalid_arguments");
    assert_eq!(mock.requests().len(), 1);

    // end_call only records the request; the application hangs up after the farewell.
    assert!(!bound.hangup_requested());
    let (is_error, _) = run_tool(&engine, 4, "end_call", json!({})).await;
    assert!(!is_error);
    assert!(bound.hangup_requested());
    assert_eq!(mock.requests().len(), 1);
    bound.hangup(9).await.unwrap();
    assert_eq!(
        mock.requests()[1].form_get("Status").as_deref(),
        Some("completed")
    );
}

#[tokio::test]
async fn the_dtmf_tool_over_twilio_without_a_resume_stream_tells_the_model_it_is_unavailable() {
    let mock = twilio_ok_mock().await;
    let control = Arc::new(twilio(&mock, |_| {}));
    let bound = Arc::new(BoundCall::new(control, caller()));
    let engine = tool_engine(&bound, &TelephonyPolicy::new());
    let (is_error, body) = run_tool(&engine, 1, "send_dtmf", json!({ "digits": "1234" })).await;
    assert!(is_error);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("not available on this call"),
        "{body}"
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn a_warm_transfer_over_telnyx_through_the_tools_is_completed_by_the_application() {
    let mock = Mock::start(|r| match r.path.as_str() {
        "/v2/conferences" => ok(json!({ "data": { "id": "conf-77" } })),
        "/v2/calls" => ok(json!({ "data": { "call_control_id": AGENT_CC } })),
        _ => telnyx_ok(),
    })
    .await;
    let control = Arc::new(telnyx(&mock, |c| {
        c.connection_id = Some("conn-42".to_owned());
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    }));
    let bound = Arc::new(BoundCall::new(control, tcall()));
    let policy = TelephonyPolicy::new()
        .destination("manager", "+905559998877", "the duty manager")
        .unwrap()
        .with_warm(true);
    let engine = tool_engine(&bound, &policy);

    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm", "summary": "refund" }),
    )
    .await;
    assert!(!is_error, "{body}");
    assert_eq!(body["mode"], "warm");
    assert_eq!(
        body["summary_delivered"], false,
        "Telnyx cannot read the summary"
    );
    let transfer = bound
        .warm_transfer()
        .expect("the transfer is kept for the application");
    assert_eq!(transfer.conference, "conf-77");

    bound.complete_warm().await.unwrap();
    assert_eq!(
        mock.requests().last().unwrap().path,
        "/v2/conferences/conf-77/actions/join"
    );
    assert!(
        bound.warm_transfer().is_none(),
        "a completed transfer is over: nothing is left to abort"
    );
    assert_eq!(bound.abort_warm().await.unwrap(), CallerState::Unchanged);
}

fn telnyx_warm_mock() -> impl Fn(&Recorded) -> Answer + Send + Sync + 'static {
    |r| match r.path.as_str() {
        "/v2/conferences" => ok(json!({ "data": { "id": "conf-77" } })),
        "/v2/calls" => ok(json!({ "data": { "call_control_id": AGENT_CC } })),
        _ => telnyx_ok(),
    }
}

fn warm_policy() -> TelephonyPolicy {
    TelephonyPolicy::new()
        .destination("manager", "+905559998877", "the duty manager")
        .unwrap()
        .with_warm(true)
}

fn telnyx_warm_control(mock: &Mock) -> Arc<TelnyxCallControl> {
    Arc::new(telnyx(mock, |c| {
        c.connection_id = Some("conn-42".to_owned());
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
    }))
}

#[tokio::test]
async fn a_failed_abort_keeps_the_transfer_so_it_can_be_retried() {
    let hangups = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hangups);
    let base = telnyx_warm_mock();
    let mock = Mock::start(move |r| {
        if r.path.ends_with("/actions/hangup") && counter.fetch_add(1, Ordering::SeqCst) == 0 {
            status(503, json!({ "errors": [{ "code": "10001" }] }))
        } else {
            base(r)
        }
    })
    .await;
    let bound = Arc::new(BoundCall::new(telnyx_warm_control(&mock), tcall()));
    let engine = tool_engine(&bound, &warm_policy());
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(!is_error, "{body}");

    // The first abort fails (503): the record must survive ...
    assert!(bound.abort_warm().await.is_err());
    assert!(
        bound.warm_transfer().is_some(),
        "the handle is kept until the abort succeeded"
    );
    // ... so that the retry reaches the provider again instead of reporting a success it never had.
    assert_eq!(bound.abort_warm().await.unwrap(), CallerState::Parked);
    assert_eq!(hangups.load(Ordering::SeqCst), 2);
    assert!(bound.warm_transfer().is_none());
    // Nothing left: further aborts do not touch the provider.
    assert_eq!(bound.abort_warm().await.unwrap(), CallerState::Unchanged);
    assert_eq!(hangups.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_aborted_tool_task_does_not_lose_the_warm_transfer_handle() {
    // The first REST step is slow. The tool future is dropped (the engine's backstop, a dropped
    // engine) after the caller was moved; the transfer still finishes and is recorded.
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let base = telnyx_warm_mock();
    let mock = Mock::start(move |r| {
        let answer = base(r);
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            answer.after(Duration::from_millis(300))
        } else {
            answer
        }
    })
    .await;
    let bound = Arc::new(BoundCall::new(telnyx_warm_control(&mock), tcall()));
    let ops: Arc<dyn TelephonyOps> = bound.clone();
    let request = request(TransferMode::Warm, Some("refund"));
    let future = ops.transfer(&request);
    let dropped = tokio::time::timeout(Duration::from_millis(80), future).await;
    assert!(dropped.is_err(), "the tool future was dropped mid-transfer");
    mock.wait_for(2, Duration::from_secs(3)).await;
    let start = Instant::now();
    while bound.warm_transfer().is_none() {
        assert!(start.elapsed() < Duration::from_secs(3), "never recorded");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let handle = bound.warm_transfer().unwrap();
    assert_eq!(handle.conference, "conf-77");
    // complete_warm and abort_warm work on it.
    bound.complete_warm().await.unwrap();
    assert!(
        mock.requests()
            .last()
            .unwrap()
            .path
            .ends_with("/conferences/conf-77/actions/join")
    );
}

#[tokio::test]
async fn a_warm_transfer_that_failed_halfway_leaves_the_bound_call_a_recovery_handle() {
    // The dial fails: the caller waits alone in the conference. The bound call keeps the handle
    // so the application can abort (and learn that the caller is parked).
    let mock = Mock::start(|r| {
        if r.path == "/v2/calls" {
            telnyx_error("10007")
        } else {
            ok(json!({ "data": { "id": "conf-5" } }))
        }
    })
    .await;
    let bound = Arc::new(BoundCall::new(telnyx_warm_control(&mock), tcall()));
    let engine = tool_engine(&bound, &warm_policy());
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error, "{body}");
    let handle = bound.warm_transfer().expect("the recovery handle");
    assert_eq!(
        (handle.conference.as_str(), handle.agent_leg),
        ("conf-5", None)
    );
    assert_eq!(bound.abort_warm().await.unwrap(), CallerState::Parked);
    assert!(bound.warm_transfer().is_none());
}

#[tokio::test]
async fn a_bound_call_without_warm_support_refuses_warm_before_any_request() {
    let mock = twilio_ok_mock().await;
    let control = Arc::new(twilio(&mock, |_| {}));
    let bound = Arc::new(BoundCall::new(control, caller()));
    let policy = TelephonyPolicy::new()
        .destination("manager", "+905559998877", "")
        .unwrap()
        .with_warm(true);
    let engine = tool_engine(&bound, &policy);
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error);
    assert!(
        body["detail"].as_str().unwrap().contains("not configured"),
        "{body}"
    );
    assert!(mock.requests().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Review fixes: stream stopped for nothing, abort on a finished leg, error mapping,
// rollback failure, settle and retention, doc claims
// ---------------------------------------------------------------------------------------------

fn steps(sent: &[Recorded]) -> Vec<&str> {
    sent.iter()
        .map(|r| r.path.rsplit('/').next().unwrap())
        .collect()
}

/// A mock whose first `transfer` is refused with 90045 and whose retry is refused with `10007`
/// (a definite 422); every other command succeeds.
async fn telnyx_retry_fails_mock() -> Mock {
    let transfers = Arc::new(AtomicUsize::new(0));
    Mock::start(move |r| {
        if r.path.ends_with("/actions/transfer") {
            if transfers.fetch_add(1, Ordering::SeqCst) == 0 {
                telnyx_error("90045")
            } else {
                telnyx_error("10007")
            }
        } else {
            telnyx_ok()
        }
    })
    .await
}

#[tokio::test]
async fn telnyx_a_failed_retry_after_the_stream_was_stopped_is_an_unknown_outcome() {
    let mock = telnyx_retry_fails_mock().await;
    let log = Arc::clone(&mock.requests);
    let hooked: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&hooked);
    let control = telnyx(&mock, move |c| {
        c.on_stream_stop = Some(StreamStopHook::new(move |call| {
            sink.lock()
                .unwrap()
                .push((call.as_str().to_owned(), log.lock().unwrap().len()));
        }));
    });
    let error = control
        .transfer_cold(&tcall(), &request(TransferMode::Cold, None))
        .await
        .unwrap_err();
    // The stream is gone: the bot cannot talk on this call, whatever the cause was.
    match &error {
        Error::StreamStopped { cause, .. } => {
            assert!(
                matches!(**cause, Error::Rejected { status: 422, .. }),
                "{cause}"
            );
        }
        other => panic!("{other}"),
    }
    assert!(error.outcome_unknown());
    assert_eq!(error.kind(), TelephonyErrorKind::OutcomeUnknown);
    assert_eq!(
        steps(&mock.requests()),
        ["transfer", "streaming_stop", "transfer"]
    );
    // The host was told about the stop before the stop command was sent (one request so far).
    assert_eq!(*hooked.lock().unwrap(), [(CC_ID.to_owned(), 1)]);

    // Without a conflict the hook is never called.
    let mock = Mock::start(|_| telnyx_ok()).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let control = telnyx(&mock, move |c| {
        c.on_stream_stop = Some(StreamStopHook::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
    });
    control
        .transfer_cold(&tcall(), &request(TransferMode::Cold, None))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn telnyx_cold_transfer_tool_after_a_stopped_stream_does_not_say_nothing_was_done() {
    let mock = telnyx_retry_fails_mock().await;
    let bound = Arc::new(BoundCall::new(Arc::new(telnyx(&mock, |_| {})), tcall()));
    let policy = TelephonyPolicy::new()
        .destination("sales", "+905551112233", "the sales team")
        .unwrap();
    let engine = tool_engine(&bound, &policy);
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(is_error, "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("do not repeat") && !detail.contains("Nothing was done"),
        "{detail}"
    );
    assert!(detail.contains("media stream was stopped"), "{detail}");
    assert_eq!(mock.requests().len(), 3);
    // The claim is kept: a second transfer is refused without reaching the provider.
    let (is_error, _) = run_tool(
        &engine,
        2,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(is_error);
    assert_eq!(mock.requests().len(), 3, "no second transfer");
}

#[tokio::test]
async fn telnyx_conference_step_retries_after_a_stream_stop_like_the_cold_transfer() {
    let conferences = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&conferences);
    let mock = Mock::start(move |r| match r.path.as_str() {
        "/v2/conferences" => {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                telnyx_error("90045")
            } else {
                ok(json!({ "data": { "id": "conf-retry" } }))
            }
        }
        "/v2/calls" => ok(json!({ "data": { "call_control_id": AGENT_CC } })),
        _ => telnyx_ok(),
    })
    .await;
    let control = telnyx_warm_control(&mock);
    let transfer = control
        .transfer_warm(&tcall(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    assert_eq!(transfer.conference, "conf-retry");
    let sent = mock.requests();
    assert_eq!(
        sent.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/v2/conferences",
            "/v2/calls/v3:caller-call-control-id/actions/streaming_stop",
            "/v2/conferences",
            "/v2/calls",
        ]
    );
    assert_eq!(
        sent[1].json()["command_id"],
        "brisvox-0000000000000abc.stop"
    );
    // The retry carries the same conference request (same name and caller).
    assert_eq!(sent[0].json(), sent[2].json());
    assert_eq!(sent[2].json()["call_control_id"], CC_ID);
}

#[tokio::test]
async fn telnyx_a_failed_conference_retry_after_the_stream_stop_parks_the_caller() {
    let conferences = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&conferences);
    let mock = Mock::start(move |r| match r.path.as_str() {
        "/v2/conferences" => {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                telnyx_error("90045")
            } else {
                telnyx_error("10007")
            }
        }
        _ => telnyx_ok(),
    })
    .await;
    let bound = Arc::new(BoundCall::new(telnyx_warm_control(&mock), tcall()));
    let engine = tool_engine(&bound, &warm_policy());
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error, "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("do not repeat") && !detail.contains("Nothing was done"),
        "{detail}"
    );
    assert_eq!(
        steps(&mock.requests()),
        ["conferences", "streaming_stop", "conferences"]
    );
    // No conference id is known, so there is no recovery handle; the error says the caller is
    // parked (the bot's stream is gone).
    assert!(bound.warm_transfer().is_none());
    let (is_error, _) = run_tool(
        &engine,
        2,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error);
    assert_eq!(
        mock.requests().len(),
        3,
        "the claim is kept: no new request"
    );

    // The same at the control level.
    let conferences = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&conferences);
    let mock = Mock::start(move |r| {
        if r.path == "/v2/conferences" {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                telnyx_error("90045")
            } else {
                telnyx_error("10007")
            }
        } else {
            telnyx_ok()
        }
    })
    .await;
    let error = telnyx_warm_control(&mock)
        .transfer_warm(&tcall(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    match &error {
        Error::WarmFailed {
            cause,
            caller,
            transfer,
        } => {
            assert_eq!(*caller, CallerState::Parked);
            assert!(transfer.is_none());
            assert!(matches!(**cause, Error::StreamStopped { .. }), "{cause}");
        }
        other => panic!("{other}"),
    }
    assert_eq!(error.kind(), TelephonyErrorKind::OutcomeUnknown);
}

/// A Twilio mock for abort tests: the caller's updates succeed, the agent leg refuses
/// `Status=completed` with a 400 (the leg is over), and `GET` of the agent leg says `leg_status`.
async fn twilio_finished_leg_mock(leg_status: &'static str) -> Mock {
    Mock::start(move |r| {
        if r.path.ends_with("/Calls.json") {
            status(201, json!({ "sid": AGENT_SID, "status": "queued" }))
        } else if r.path == update_path(AGENT_SID) {
            if r.method == "GET" {
                ok(json!({ "sid": AGENT_SID, "status": leg_status }))
            } else if r.form_get("Status").as_deref() == Some("completed") {
                status(
                    400,
                    json!({ "code": 21220, "message": "Call is not in-progress. Cannot redirect." }),
                )
            } else {
                ok(json!({ "sid": AGENT_SID, "status": "canceled" }))
            }
        } else {
            ok(json!({ "sid": CALLER_SID, "status": "in-progress" }))
        }
    })
    .await
}

fn twilio_resuming(mock: &Mock) -> TwilioCallControl {
    twilio(mock, |c| {
        c.caller_id = Some(PhoneNumber::parse("+908501234567").unwrap());
        c.resume_stream = Some(resume());
    })
}

#[tokio::test]
async fn twilio_abort_resumes_the_caller_when_the_agent_leg_already_ended_with_a_4xx() {
    for final_status in ["no-answer", "busy", "failed", "completed", "canceled"] {
        let mock = twilio_finished_leg_mock(final_status).await;
        let control = twilio_resuming(&mock);
        let transfer = control
            .transfer_warm(&caller(), &request(TransferMode::Warm, None))
            .await
            .unwrap();
        let state = control.abort_warm(&transfer).await.unwrap();
        assert_eq!(state, CallerState::Resumed, "{final_status}");
        let sent = mock.requests();
        // park, create, end leg (400), read leg, resume caller
        assert_eq!(sent.len(), 5, "{final_status}");
        assert_eq!(
            (sent[2].method.as_str(), sent[2].path.as_str()),
            ("POST", update_path(AGENT_SID).as_str())
        );
        assert_eq!(sent[3].method, "GET");
        assert_eq!(sent[4].path, update_path(CALLER_SID));
        assert!(
            sent[4]
                .form_get("Twiml")
                .unwrap()
                .contains("value=\"warm_failed\""),
            "{final_status}"
        );
    }
}

#[tokio::test]
async fn twilio_abort_cancels_a_ringing_agent_leg() {
    // `completed` is refused for a call that is still ringing; `canceled` is the status for it.
    let mock = twilio_finished_leg_mock("ringing").await;
    let control = twilio_resuming(&mock);
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    assert_eq!(
        control.abort_warm(&transfer).await.unwrap(),
        CallerState::Resumed
    );
    let sent = mock.requests();
    assert_eq!(sent.len(), 6);
    assert_eq!(sent[2].form_get("Status").as_deref(), Some("completed"));
    assert_eq!(sent[3].method, "GET");
    assert_eq!(sent[4].path, update_path(AGENT_SID));
    assert_eq!(sent[4].form_get("Status").as_deref(), Some("canceled"));
    assert_eq!(sent[5].path, update_path(CALLER_SID));
}

#[tokio::test]
async fn twilio_abort_keeps_the_caller_parked_while_a_live_agent_leg_refuses_to_end() {
    // The refusal is not explained by a finished leg: the caller must not be resumed behind a
    // live agent leg, and the error is returned so that the abort can be retried.
    let mock = twilio_finished_leg_mock("in-progress").await;
    let control = twilio_resuming(&mock);
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    let error = control.abort_warm(&transfer).await.unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 400, .. }),
        "{error}"
    );
    let sent = mock.requests();
    assert_eq!(sent.len(), 4, "the caller was not resumed: {sent:?}");
    assert_eq!(sent[3].method, "GET");

    // An unreadable status (the read fails too) is the same: original refusal, caller untouched.
    let mock = Mock::start(|r| {
        if r.path.ends_with("/Calls.json") {
            status(201, json!({ "sid": AGENT_SID }))
        } else if r.path == update_path(AGENT_SID) {
            if r.method == "GET" {
                status(500, json!({}))
            } else {
                status(400, json!({ "code": 21220 }))
            }
        } else {
            ok(json!({ "sid": CALLER_SID }))
        }
    })
    .await;
    let control = twilio_resuming(&mock);
    let transfer = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap();
    let error = control.abort_warm(&transfer).await.unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 400, .. }),
        "{error}"
    );
    assert_eq!(mock.requests().len(), 4);
}

#[tokio::test]
async fn a_cleanly_rolled_back_warm_transfer_frees_the_model_to_try_again() {
    // Twilio: the agent call is refused (400), the caller is put back on the bot: nothing was
    // done and the call continues with the model.
    let mock = Mock::start(|r| {
        if r.path.ends_with("/Calls.json") {
            status(
                400,
                json!({ "code": 21211, "message": "Invalid 'To' Phone Number" }),
            )
        } else {
            ok(json!({ "sid": CALLER_SID, "status": "in-progress" }))
        }
    })
    .await;
    let bound = Arc::new(BoundCall::new(Arc::new(twilio_resuming(&mock)), caller()));
    let engine = tool_engine(&bound, &warm_policy());
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error, "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("Nothing was done") && !detail.contains("do not repeat"),
        "{detail}"
    );
    assert!(bound.warm_transfer().is_none(), "nothing to recover");
    assert_eq!(mock.requests().len(), 3, "park, create, resume");
    // The claim was released: a second transfer reaches the provider again.
    let (is_error, _) = run_tool(
        &engine,
        2,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error);
    assert!(
        mock.requests().len() > 3,
        "the second transfer was sent: {}",
        mock.requests().len()
    );
}

#[tokio::test]
async fn a_parked_warm_transfer_tells_the_model_not_to_repeat_and_refuses_a_second_one() {
    let mock = Mock::start(|r| {
        if r.path == "/v2/calls" {
            telnyx_error("10007")
        } else {
            ok(json!({ "data": { "id": "conf-5" } }))
        }
    })
    .await;
    let bound = Arc::new(BoundCall::new(telnyx_warm_control(&mock), tcall()));
    let engine = tool_engine(&bound, &warm_policy());
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error, "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("do not repeat") && !detail.contains("Nothing was done"),
        "{detail}"
    );
    let before = mock.requests().len();
    let (is_error, _) = run_tool(
        &engine,
        2,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error);
    assert_eq!(mock.requests().len(), before, "refused: 0 new requests");
}

#[tokio::test]
async fn twilio_rollback_that_fails_leaves_the_caller_parked_with_a_recovery_handle() {
    // The agent call is refused (400) and then the resume update fails (500): the caller is
    // stranded alone in the conference, and the handle must say so.
    let caller_updates = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&caller_updates);
    let mock = Mock::start(move |r| {
        if r.path.ends_with("/Calls.json") {
            status(
                400,
                json!({ "code": 21211, "message": "Invalid 'To' Phone Number" }),
            )
        } else if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            // The park update.
            ok(json!({ "sid": CALLER_SID }))
        } else if counter.load(Ordering::SeqCst) == 2 {
            // The resume update of the rollback.
            status(500, json!({}))
        } else {
            ok(json!({ "sid": CALLER_SID }))
        }
    })
    .await;
    let control = twilio_resuming(&mock);
    let error = control
        .transfer_warm(&caller(), &request(TransferMode::Warm, None))
        .await
        .unwrap_err();
    match &error {
        Error::WarmFailed {
            cause,
            caller,
            transfer,
        } => {
            assert_eq!(*caller, CallerState::Parked);
            assert!(
                matches!(**cause, Error::Rejected { status: 400, .. }),
                "{cause}"
            );
            let handle = transfer.as_ref().expect("a recovery handle");
            assert_eq!(handle.agent_leg, None);
            assert_eq!(handle.conference, "brisvox-0000000000000abc");
        }
        other => panic!("{other}"),
    }
    assert_eq!(error.kind(), TelephonyErrorKind::OutcomeUnknown);
    assert_eq!(mock.requests().len(), 3, "park, create, failed resume");

    // Through the bound call: the handle is kept, and an abort retries the resume.
    caller_updates.store(0, Ordering::SeqCst);
    let bound = Arc::new(BoundCall::new(Arc::new(twilio_resuming(&mock)), caller()));
    let engine = tool_engine(&bound, &warm_policy());
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(is_error, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("do not repeat"),
        "{body}"
    );
    let handle = bound
        .warm_transfer()
        .expect("the bound call keeps the handle");
    assert_eq!(handle.agent_leg, None);
    // The mock answers resume updates with 200 from now on.
    assert_eq!(bound.abort_warm().await.unwrap(), CallerState::Resumed);
    assert!(bound.warm_transfer().is_none());
}

#[tokio::test]
async fn the_bound_call_waits_for_the_settle_function_before_it_transfers() {
    // The hand-off sentence must be played before the transfer is sent: the transfer request
    // must arrive after the settle function finished (and not before).
    let settled = Arc::new(AtomicBool::new(false));
    let seen_settled = Arc::new(AtomicUsize::new(usize::MAX));
    let (flag, seen) = (Arc::clone(&settled), Arc::clone(&seen_settled));
    let mock = Mock::start(move |_| {
        seen.store(usize::from(flag.load(Ordering::SeqCst)), Ordering::SeqCst);
        ok(json!({ "sid": CALLER_SID, "status": "in-progress" }))
    })
    .await;
    let done = Arc::clone(&settled);
    let settle: SettleFn = Arc::new(move || {
        let done = Arc::clone(&done);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            done.store(true, Ordering::SeqCst);
        })
    });
    let bound =
        Arc::new(BoundCall::new(Arc::new(twilio(&mock, |_| {})), caller()).with_settle(settle));
    // Directly: settle_speech returns only once the function completed.
    let start = Instant::now();
    bound.settle_speech().await.unwrap();
    assert!(settled.load(Ordering::SeqCst));
    assert!(start.elapsed() >= Duration::from_millis(200));

    // Through the tool: the provider is not reached before the settle function finished.
    settled.store(false, Ordering::SeqCst);
    let policy = TelephonyPolicy::new()
        .destination("sales", "+905551112233", "the sales team")
        .unwrap();
    let engine = tool_engine(&bound, &policy);
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "sales" }),
    )
    .await;
    assert!(!is_error, "{body}");
    assert_eq!(
        seen_settled.load(Ordering::SeqCst),
        1,
        "the transfer request was sent before the hand-off sentence was settled"
    );

    // Without a settle function the wait is skipped.
    let bound = BoundCall::new(Arc::new(twilio(&mock, |_| {})), caller());
    let start = Instant::now();
    bound.settle_speech().await.unwrap();
    assert!(start.elapsed() < Duration::from_millis(100));
}

#[tokio::test]
async fn a_failed_join_keeps_the_warm_transfer_so_the_join_can_be_retried() {
    let joins = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&joins);
    let base = telnyx_warm_mock();
    let mock = Mock::start(move |r| {
        if r.path.ends_with("/actions/join") && counter.fetch_add(1, Ordering::SeqCst) == 0 {
            status(503, json!({ "errors": [{ "code": "10001" }] }))
        } else {
            base(r)
        }
    })
    .await;
    let bound = Arc::new(BoundCall::new(telnyx_warm_control(&mock), tcall()));
    let engine = tool_engine(&bound, &warm_policy());
    let (is_error, body) = run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(!is_error, "{body}");

    let error = bound.complete_warm().await.unwrap_err();
    assert!(error.outcome_unknown(), "{error}");
    assert!(
        bound.warm_transfer().is_some(),
        "the transfer survives a failed join"
    );
    // abort_warm still works on it (it would end the agent leg), and the join can be repeated.
    bound.complete_warm().await.unwrap();
    assert_eq!(
        joins.load(Ordering::SeqCst),
        2,
        "the retry reached the provider"
    );
    assert!(bound.warm_transfer().is_none(), "forgotten once joined");
    // Nothing in progress: a further call is a no-op.
    bound.complete_warm().await.unwrap();
    assert_eq!(joins.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_failed_join_leaves_abort_warm_able_to_end_the_agent_leg() {
    let base = telnyx_warm_mock();
    let mock = Mock::start(move |r| {
        if r.path.ends_with("/actions/join") {
            status(503, json!({ "errors": [{ "code": "10001" }] }))
        } else {
            base(r)
        }
    })
    .await;
    let bound = Arc::new(BoundCall::new(telnyx_warm_control(&mock), tcall()));
    let engine = tool_engine(&bound, &warm_policy());
    run_tool(
        &engine,
        1,
        "transfer_call",
        json!({ "destination": "manager", "mode": "warm" }),
    )
    .await;
    assert!(bound.complete_warm().await.is_err());
    // A mutation that took the slot before the join succeeded would report `Unchanged` here and
    // never hang the agent leg up.
    assert_eq!(bound.abort_warm().await.unwrap(), CallerState::Parked);
    assert!(
        mock.requests()
            .last()
            .unwrap()
            .path
            .ends_with("/actions/hangup")
    );
}

#[tokio::test]
async fn redirects_are_never_followed() {
    // A 307 would resend the POST (and its credentials) to the Location. Exactly one request
    // must reach the mock, and the reply is an error.
    let mock = Mock::start(|r| {
        if r.path.ends_with("/elsewhere") {
            ok(json!({ "data": { "result": "ok" } }))
        } else {
            status(307, json!({})).redirect_to("/elsewhere")
        }
    })
    .await;
    let control = twilio(&mock, |_| {});
    let error = control.hangup(&caller(), 1).await.unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 307, .. }),
        "{error}"
    );
    let control = telnyx(&mock, |_| {});
    let error = control.hangup(&tcall(), 1).await.unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 307, .. }),
        "{error}"
    );
    let sent = mock.requests();
    assert_eq!(sent.len(), 2, "one request each, no follow-up: {sent:?}");
    assert!(sent.iter().all(|r| !r.path.ends_with("/elsewhere")));

    // 302 on a GET.
    let mock = Mock::start(|r| {
        if r.path.ends_with("/elsewhere") {
            ok(json!({ "status": "completed" }))
        } else {
            status(302, json!({})).redirect_to("/elsewhere")
        }
    })
    .await;
    let error = twilio(&mock, |_| {})
        .call_status(&caller())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Rejected { status: 302, .. }),
        "{error}"
    );
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn provider_error_text_is_cut_to_200_characters() {
    let long = "x".repeat(5000);
    let text = long.clone();
    let mock = Mock::start(move |r| {
        if r.path.starts_with("/v2") {
            status(
                422,
                json!({ "errors": [{ "code": "10004", "detail": text }] }),
            )
        } else {
            status(400, json!({ "code": 21211, "message": text }))
        }
    })
    .await;
    let error = twilio(&mock, |_| {})
        .hangup(&caller(), 1)
        .await
        .unwrap_err();
    match error {
        Error::Rejected { detail, .. } => assert_eq!(detail.chars().count(), 200),
        other => panic!("{other}"),
    }
    let error = telnyx(&mock, |_| {}).hangup(&tcall(), 1).await.unwrap_err();
    match error {
        Error::Rejected { detail, .. } => assert_eq!(detail.chars().count(), 200),
        other => panic!("{other}"),
    }
}

/// A clock that runs a thousand times faster than real time: a sleep of `d` takes `d / 1000`.
/// Only a deadline taken from the injected clock is shortened; a deadline from `tokio::time`
/// directly would still take its full real length.
#[derive(Debug)]
struct FastClock;

impl Clock for FastClock {
    fn now(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    fn wall(&self) -> std::time::SystemTime {
        std::time::SystemTime::now()
    }

    fn sleep_until(&self, deadline: tokio::time::Instant) -> tokio::time::Sleep {
        let now = tokio::time::Instant::now();
        tokio::time::sleep_until(now + deadline.saturating_duration_since(now) / 1000)
    }
}

#[tokio::test]
async fn the_request_deadline_runs_on_the_injected_clock() {
    // The server answers after 20 s of real time; the configured deadline is 300 s on the
    // injected clock, which is 300 ms of real time. The timeout must therefore come from the
    // clock, in well under 5 s; a `tokio::time::timeout(300 s)` would not.
    let mock = Mock::start(|_| telnyx_ok().after(Duration::from_secs(20))).await;
    let mut config = telnyx_config(&mock);
    config.request_timeout = Duration::from_secs(300);
    let control = TelnyxCallControl::new(config, Arc::new(FastClock)).unwrap();
    let start = Instant::now();
    let error = tokio::time::timeout(Duration::from_secs(5), control.hangup(&tcall(), 1))
        .await
        .expect("the deadline must come from the injected clock")
        .unwrap_err();
    assert!(
        matches!(error, Error::Timeout { after, .. } if after == Duration::from_secs(300)),
        "{error}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
}
