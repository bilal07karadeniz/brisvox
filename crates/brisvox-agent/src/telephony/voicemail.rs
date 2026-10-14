//! Voicemail and answering-machine handling for outbound calls.
//!
//! # Strategy (`docs/TELEPHONY.md`, "Answering machines")
//! Detection layers, in order of authority:
//! 1. **Provider AMD.** The provider classifies the answer (Twilio `MachineDetection` /
//!    `AsyncAmd`, Telnyx `answering_machine_detection`) and reports an [`AnsweredBy`]. The
//!    transport crate parses the callbacks into that type. Leaving a message needs the *end of
//!    the greeting*: Twilio `DetectMessageEnd` or Telnyx `greeting_end` / `premium`. The early
//!    modes (Twilio `Enable`, Telnyx `detect`) report a machine but never when its greeting ends;
//!    use [`VoicemailPolicy::detect_only`] with them (it hangs up on machines instead of
//!    guessing, and `AmdConfig::twilio_provides_greeting_end` and `telnyx_provides_greeting_end` in the transport crate say which
//!    configuration needs which policy).
//! 2. **In-engine hint** ([`voicemail_hint`] / [`looks_like_voicemail`], fed in with
//!    [`VoicemailDriver::on_voicemail_hint`]): voicemail phrases in the callee's first words
//!    ([`HintStrength::Phrase`]), or a long single utterance ([`HintStrength::Length`]). Cheap and
//!    language-aware. It never turns a machine into a human. It **upgrades** a verdict that was
//!    too optimistic, and the two strengths are not equal: a *phrase* hint settles an `Unknown`
//!    verdict as "machine" and turns a `Human` verdict into "machine" (when the hint arrives
//!    within [`VoicemailPolicy::human_upgrade_window`] after the verdict, or up to that long
//!    before it: Twilio `DetectMessageEnd` answers `human` for a short greeting that then goes on
//!    with "leave a message after the beep", and the speech-to-text final may be faster than the
//!    verdict). A *length* hint (an utterance longer than 3.5 s with more than 5 words, which a
//!    live person with a long "hello, this is the Smith residence, how can I help you" also
//!    produces) only settles an `Unknown` verdict; it never overrides `Human`. The window and the
//!    specific phrases are the guard against talking to a voicemail script over a real person.
//!    After an upgrade the driver waits [`VoicemailPolicy::hint_wait`] for the greeting to end
//!    (the provider will not report it) and then speaks.
//! 3. **Fallback** when the provider says `Unknown`: [`VoicemailPolicy::on_unknown`] (treat as a
//!    person by default, the Telnyx guide's advice for `not_sure`). `TreatAsMachine` does not
//!    speak at once, because the greeting may still be playing (an `Unknown` comes from a
//!    detection timeout): it waits [`VoicemailPolicy::hint_wait`] like every other machine path
//!    without a greeting-end event, then speaks (or hangs up with
//!    `speak_when_greeting_end_uncertain` off). Beep detection inside the engine (DSP on the
//!    callee audio) is future work.
//!
//! # The flow
//! ```text
//! classify --Human--------------------------------------> human   (conversation)
//!    |--MachineStart--> wait_greeting --MachineEnd*------> leave_message (Say, EndCall)
//!    |--MachineEnd*--------------------------------------> leave_message
//!    '--Fax / policy--> hang up
//! human --phrase hint or MachineEnd*, within the window--> wait_greeting / leave_message
//! ```
//! [`voicemail_flow`] builds the nodes (a [`Flow`]). The nodes have no model turn of
//! their own (`respond_immediately` is off except for `human`): the *verdicts* come from the
//! application, not from the model, so the transitions are driven by [`VoicemailDriver`] and
//! applied with [`FlowManager::transition_to`](crate::FlowManager::transition_to). `leave_message`
//! enters with three actions in order, which the runner performs in order
//! (the action contract is on [`Action`]):
//! `Custom("barge_in", {"enabled": false})`, `Say(message)`, `EndCall`. `EndCall` after a `Say`
//! hangs up once the utterance-end mark of the message was played (with the runner's own
//! timeout), not immediately. The message is fixed text from the configuration: no model turn, no
//! STT-driven interruption, nothing the callee said can change it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::time::Instant;

use super::AnsweredBy;
use crate::flows::{
    Action, ContextStrategy, Flow, FlowBuilder, FlowDefError, Node, NodeId, ToolBinding,
};
use crate::segment::Language;
use crate::tools::ToolRegistry;

/// Node id: waiting for the verdict. The model does not speak.
pub const NODE_CLASSIFY: &str = "classify";
/// Node id: a person answered; normal conversation.
pub const NODE_HUMAN: &str = "human";
/// Node id: a machine answered and its greeting is still playing. The model does not speak.
pub const NODE_WAIT_GREETING: &str = "wait_greeting";
/// Node id: speak the message, hang up.
pub const NODE_LEAVE_MESSAGE: &str = "leave_message";
/// Node id: the farewell after `end_call` (only when the registry has `end_call`).
pub const NODE_FAREWELL: &str = "farewell";
/// Name of the [`Action::Custom`] that switches barge-in on or off (payload `{"enabled": bool}`).
pub const ACTION_BARGE_IN: &str = "barge_in";

/// Default longest wait for the provider's first verdict: 45 seconds (Twilio's
/// `MachineDetectionTimeout` must stay below 59 and "30 seconds is frequently not enough" for
/// business voicemail).
pub const DEFAULT_CLASSIFY_TIMEOUT: Duration = Duration::from_secs(45);
/// Default longest wait for the end of a machine greeting: 30 seconds (Telnyx's beep-detection
/// window).
pub const DEFAULT_GREETING_TIMEOUT: Duration = Duration::from_secs(30);

/// What to do when the detector cannot tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fallback {
    /// Talk as if a person answered (Telnyx recommends it for standard AMD `not_sure`).
    #[default]
    TreatAsHuman,
    /// Leave the message as if a machine answered.
    TreatAsMachine,
    /// Hang up.
    HangUp,
}

/// Default window after a `Human` verdict in which a voicemail hint or a late machine verdict
/// still upgrades the call to "machine": 10 seconds.
pub const DEFAULT_HUMAN_UPGRADE_WINDOW: Duration = Duration::from_secs(10);
/// Default wait for the end of a greeting that the provider cannot report (after an in-engine
/// hint, or with a detect-only provider mode): 6 seconds.
pub const DEFAULT_HINT_WAIT: Duration = Duration::from_secs(6);

/// Driver settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoicemailPolicy {
    /// Longest wait for the first verdict.
    pub classify_timeout: Duration,
    /// Longest wait for the end of the greeting after `MachineStart`.
    pub greeting_timeout: Duration,
    /// What to do on an `Unknown` verdict or a classification timeout.
    pub on_unknown: Fallback,
    /// Whether to speak when the greeting end is uncertain (an `Unknown` verdict while waiting for
    /// the end of the greeting, or the greeting timeout). `false` hangs up instead.
    pub speak_when_greeting_end_uncertain: bool,
    /// Whether the provider reports the end of a machine's greeting (Twilio `DetectMessageEnd`,
    /// Telnyx `greeting_end` / `premium`). With `false` (Twilio `Enable`, Telnyx `detect`) a
    /// `MachineStart` is the last event that will ever come: the driver hangs up on the machine,
    /// or with `speak_when_greeting_end_uncertain` waits [`Self::hint_wait`] and speaks. Use
    /// [`Self::detect_only`].
    pub greeting_end_events: bool,
    /// How long after a `Human` verdict a voicemail hint or a late machine verdict still turns
    /// the call into a machine call. Zero disables the upgrade.
    pub human_upgrade_window: Duration,
    /// How long to wait for the end of a greeting that no event will announce (after an upgrade by
    /// a hint, or with `greeting_end_events` off) before speaking.
    pub hint_wait: Duration,
}

impl Default for VoicemailPolicy {
    fn default() -> Self {
        Self {
            classify_timeout: DEFAULT_CLASSIFY_TIMEOUT,
            greeting_timeout: DEFAULT_GREETING_TIMEOUT,
            on_unknown: Fallback::TreatAsHuman,
            speak_when_greeting_end_uncertain: true,
            greeting_end_events: true,
            human_upgrade_window: DEFAULT_HUMAN_UPGRADE_WINDOW,
            hint_wait: DEFAULT_HINT_WAIT,
        }
    }
}

impl VoicemailPolicy {
    /// The policy for a provider in an early-detection mode (Twilio `MachineDetection=Enable`,
    /// Telnyx `detect`): machines are recognized but their greeting end is never reported, so
    /// the call is ended on a machine rather than speaking blindly over a greeting.
    pub fn detect_only() -> Self {
        Self {
            greeting_end_events: false,
            speak_when_greeting_end_uncertain: false,
            ..Self::default()
        }
    }
}

/// Where the driver is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the first verdict.
    Classifying,
    /// A machine answered; waiting for its greeting to end.
    WaitingGreeting,
    /// The message is being spoken.
    Leaving,
    /// A person is on the line.
    Human,
    /// Finished (message played, or hung up).
    Done,
}

/// How a call ended, for metrics (`docs/providers/call-control.md`, 3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallOutcome {
    /// A person answered and the conversation ran.
    Human,
    /// The message was played to a machine.
    MachineMessageLeft,
    /// A machine answered and the call was ended without a message.
    MachineHangUp,
    /// A fax machine answered.
    Fax,
    /// The detector could not tell and the policy hung up.
    Unknown,
}

/// The flow node to enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoicemailNode {
    /// [`NODE_HUMAN`].
    Human,
    /// [`NODE_WAIT_GREETING`].
    WaitGreeting,
    /// [`NODE_LEAVE_MESSAGE`].
    LeaveMessage,
}

impl VoicemailNode {
    /// The node id in the flow built by [`voicemail_flow`].
    pub fn node_id(self) -> NodeId {
        NodeId::new(match self {
            Self::Human => NODE_HUMAN,
            Self::WaitGreeting => NODE_WAIT_GREETING,
            Self::LeaveMessage => NODE_LEAVE_MESSAGE,
        })
    }
}

/// What the runner must do after feeding the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing.
    Stay,
    /// Enter this node (`FlowManager::transition_to`).
    Enter(VoicemailNode),
    /// End the call now (no message, or nothing to say).
    HangUp,
}

/// Maps detector verdicts and timeouts to flow steps. Pure: the caller passes `Instant`s and
/// arms a timer for [`Self::deadline`]; no clock, no IO.
#[derive(Debug, Clone)]
pub struct VoicemailDriver {
    policy: VoicemailPolicy,
    phase: Phase,
    deadline: Option<Instant>,
    outcome: Option<CallOutcome>,
    human_at: Option<Instant>,
    /// A hint of any strength came while classifying (settles an `Unknown`).
    hint_seen: bool,
    /// When a *phrase* hint came while classifying (overrides a later `Human`, within the
    /// upgrade window).
    phrase_hint_at: Option<Instant>,
}

impl VoicemailDriver {
    /// A driver for a call answered at `answered_at`.
    pub fn new(policy: VoicemailPolicy, answered_at: Instant) -> Self {
        Self {
            policy,
            phase: Phase::Classifying,
            deadline: answered_at.checked_add(policy.classify_timeout),
            outcome: None,
            human_at: None,
            hint_seen: false,
            phrase_hint_at: None,
        }
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// How the call ended, once known. [`CallOutcome::MachineMessageLeft`] only after
    /// [`Self::message_played`].
    pub fn outcome(&self) -> Option<CallOutcome> {
        self.outcome
    }

    /// The instant the runner must call [`Self::on_timeout`] at, if a wait is running.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// A verdict from the provider.
    pub fn on_verdict(&mut self, verdict: AnsweredBy, now: Instant) -> Step {
        match self.phase {
            Phase::Classifying => self.classify(verdict, now),
            Phase::WaitingGreeting => self.wait_greeting(verdict),
            Phase::Human => self.late_verdict(verdict, now),
            Phase::Leaving | Phase::Done => Step::Stay,
        }
    }

    /// The in-engine hint ([`voicemail_hint`] returned `strength` for what the callee said). It
    /// is weaker than a provider verdict.
    ///
    /// While classifying it only remembers the hint: any strength settles a later `Unknown`
    /// verdict as a machine, and a [`HintStrength::Phrase`] hint also turns a later `Human`
    /// verdict (given within [`VoicemailPolicy::human_upgrade_window`] of the hint) into a
    /// machine call. Within the human-upgrade window after a `Human` verdict a `Phrase` hint
    /// turns the call into a machine call and waits [`VoicemailPolicy::hint_wait`] for the
    /// greeting to end. A [`HintStrength::Length`] hint never overrides a `Human` verdict: a live
    /// person can say a long hello. A machine verdict already given is never reversed.
    pub fn on_voicemail_hint(&mut self, strength: HintStrength, now: Instant) -> Step {
        match self.phase {
            Phase::Classifying => {
                self.hint_seen = true;
                if strength == HintStrength::Phrase {
                    self.phrase_hint_at = Some(now);
                }
                Step::Stay
            }
            Phase::Human if strength == HintStrength::Phrase && self.in_upgrade_window(now) => {
                self.begin_wait(now, self.policy.hint_wait)
            }
            Phase::Human | Phase::WaitingGreeting | Phase::Leaving | Phase::Done => Step::Stay,
        }
    }

    /// Call when `now` reached [`Self::deadline`] (calling earlier does nothing).
    pub fn on_timeout(&mut self, now: Instant) -> Step {
        if self.deadline.is_none_or(|deadline| now < deadline) {
            return Step::Stay;
        }
        match self.phase {
            Phase::Classifying => self.unknown(now),
            Phase::WaitingGreeting => self.uncertain_greeting_end(),
            Phase::Leaving | Phase::Human | Phase::Done => Step::Stay,
        }
    }

    /// The message was played (its utterance-end mark came back): the call is done.
    pub fn message_played(&mut self) {
        if self.phase == Phase::Leaving {
            self.phase = Phase::Done;
            self.outcome = Some(CallOutcome::MachineMessageLeft);
        }
    }

    fn classify(&mut self, verdict: AnsweredBy, now: Instant) -> Step {
        match verdict {
            AnsweredBy::Human if self.recent_phrase_hint(now) => {
                // The callee already said a voicemail phrase before the verdict arrived: the
                // same upgrade as a hint after the verdict.
                self.begin_wait(now, self.policy.hint_wait)
            }
            AnsweredBy::Human => self.human(now),
            AnsweredBy::MachineStart => self.machine_started(now),
            AnsweredBy::MachineEndBeep
            | AnsweredBy::MachineEndSilence
            | AnsweredBy::MachineEndOther => self.leave(),
            AnsweredBy::Fax => self.hang_up(CallOutcome::Fax),
            AnsweredBy::Unknown => self.unknown(now),
        }
    }

    /// Whether a phrase hint heard while classifying is still within the upgrade window of `now`
    /// (a zero window disables the upgrade, as after a verdict).
    fn recent_phrase_hint(&self, now: Instant) -> bool {
        self.phrase_hint_at.is_some_and(|at| {
            at.checked_add(self.policy.human_upgrade_window)
                .is_some_and(|end| now < end)
        })
    }

    fn wait_greeting(&mut self, verdict: AnsweredBy) -> Step {
        match verdict {
            AnsweredBy::MachineEndBeep
            | AnsweredBy::MachineEndSilence
            | AnsweredBy::MachineEndOther => self.leave(),
            AnsweredBy::Fax => self.hang_up(CallOutcome::Fax),
            AnsweredBy::Unknown => self.uncertain_greeting_end(),
            // A repeated `MachineStart`, or a `Human` verdict after a machine was detected: the
            // detector contradicts itself; keep waiting for the greeting to end.
            AnsweredBy::MachineStart | AnsweredBy::Human => Step::Stay,
        }
    }

    /// A verdict after `Human`: only a machine verdict inside the upgrade window counts.
    fn late_verdict(&mut self, verdict: AnsweredBy, now: Instant) -> Step {
        if !self.in_upgrade_window(now) {
            return Step::Stay;
        }
        match verdict {
            AnsweredBy::MachineEndBeep
            | AnsweredBy::MachineEndSilence
            | AnsweredBy::MachineEndOther => self.leave(),
            AnsweredBy::MachineStart => self.machine_started(now),
            AnsweredBy::Human | AnsweredBy::Fax | AnsweredBy::Unknown => Step::Stay,
        }
    }

    fn in_upgrade_window(&self, now: Instant) -> bool {
        self.human_at.is_some_and(|at| {
            at.checked_add(self.policy.human_upgrade_window)
                .is_some_and(|end| now < end)
        })
    }

    /// A machine was detected and its greeting is playing.
    fn machine_started(&mut self, now: Instant) -> Step {
        if self.policy.greeting_end_events {
            self.begin_wait(now, self.policy.greeting_timeout)
        } else if self.policy.speak_when_greeting_end_uncertain {
            // No event will announce the end of the greeting: wait a fixed time.
            self.begin_wait(now, self.policy.hint_wait)
        } else {
            self.hang_up(CallOutcome::MachineHangUp)
        }
    }

    fn begin_wait(&mut self, now: Instant, wait: Duration) -> Step {
        self.phase = Phase::WaitingGreeting;
        self.deadline = now.checked_add(wait);
        self.outcome = None;
        self.human_at = None;
        Step::Enter(VoicemailNode::WaitGreeting)
    }

    fn unknown(&mut self, now: Instant) -> Step {
        // A voicemail hint heard while the provider was still undecided settles an `Unknown`.
        if self.hint_seen && self.policy.on_unknown != Fallback::HangUp {
            return self.begin_wait(now, self.policy.hint_wait);
        }
        match self.policy.on_unknown {
            Fallback::TreatAsHuman => self.human(now),
            // The greeting may still be playing (an `Unknown` is a detection timeout): wait for
            // it like the hint path, never speak over it.
            Fallback::TreatAsMachine => self.begin_wait(now, self.policy.hint_wait),
            Fallback::HangUp => self.hang_up(CallOutcome::Unknown),
        }
    }

    fn uncertain_greeting_end(&mut self) -> Step {
        if self.policy.speak_when_greeting_end_uncertain {
            self.leave()
        } else {
            self.hang_up(CallOutcome::MachineHangUp)
        }
    }

    fn human(&mut self, now: Instant) -> Step {
        self.phase = Phase::Human;
        self.deadline = None;
        self.outcome = Some(CallOutcome::Human);
        self.human_at = Some(now);
        Step::Enter(VoicemailNode::Human)
    }

    fn leave(&mut self) -> Step {
        self.phase = Phase::Leaving;
        self.deadline = None;
        self.outcome = None;
        self.human_at = None;
        Step::Enter(VoicemailNode::LeaveMessage)
    }

    fn hang_up(&mut self, outcome: CallOutcome) -> Step {
        self.phase = Phase::Done;
        self.deadline = None;
        self.outcome = Some(outcome);
        Step::HangUp
    }
}

/// Phrases machines use, per language (lowercase; matched on the lowercased greeting).
const PHRASES_EN: &[&str] = &[
    "leave a message",
    "leave your message",
    "after the beep",
    "after the tone",
    "voicemail",
    "voice mail",
    "cannot take your call",
    "can't take your call",
    "record your message",
    "mailbox",
];
const PHRASES_TR: &[&str] = &[
    "mesaj bırakın",
    "mesajınızı bırakın",
    "bip sesinden sonra",
    "sinyal sesinden sonra",
    "sesli mesaj",
    "sesli posta",
    "şu anda müsait değil",
    "ulaşılamıyor",
    "cevap veremiyor",
    "telesekreter",
];

/// Telnyx's documented heuristic: a greeting longer than this is a machine's
/// (`greeting_duration_millis` default 3500).
pub const MACHINE_GREETING_DURATION: Duration = Duration::from_millis(3500);
/// Telnyx's documented heuristic: more words than this in the greeting is a machine
/// (`maximum_number_of_words` default 5).
pub const MACHINE_GREETING_WORDS: usize = 5;

/// How strong a voicemail hint is (see [`voicemail_hint`]). Ordered: `Length` is weaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HintStrength {
    /// Only the length of the utterance: longer than [`MACHINE_GREETING_DURATION`] with more than
    /// [`MACHINE_GREETING_WORDS`] words. A live person's long greeting matches too, so this only
    /// settles an `Unknown` verdict and never overrides `Human`.
    Length,
    /// A voicemail phrase ("leave a message", "bip sesinden sonra", ...): specific enough to
    /// upgrade a `Human` verdict.
    Phrase,
}

/// Whether the callee's first words look like a machine's greeting, and how strongly: a voicemail
/// phrase in either language ([`HintStrength::Phrase`]), or (when `speech` is the length of the
/// first utterance) a greeting longer than [`MACHINE_GREETING_DURATION`] with more than
/// [`MACHINE_GREETING_WORDS`] words ([`HintStrength::Length`]). A heuristic from the provider's
/// own documented thresholds; the phrase lists are suggestions, tune them on real calls. `text` is
/// the callee's speech and is only compared, never executed. Feed the result to
/// [`VoicemailDriver::on_voicemail_hint`].
pub fn voicemail_hint(text: &str, speech: Option<Duration>) -> Option<HintStrength> {
    // English phrases match the ordinary lowercase (a dotted capital I from Turkish text is
    // folded to a plain i); Turkish phrases match the Turkish lowercase (I to ı, İ to i), so
    // "SESLİ MESAJ" and "BIRAKIN" both work.
    let lower: String = text
        .to_lowercase()
        .chars()
        .filter(|c| *c != '\u{307}')
        .collect();
    let lower_tr: String = text
        .chars()
        .flat_map(|c| match c {
            'I' => vec!['ı'],
            'İ' => vec!['i'],
            other => other.to_lowercase().collect(),
        })
        .collect();
    if PHRASES_EN.iter().any(|phrase| lower.contains(phrase))
        || PHRASES_TR.iter().any(|phrase| lower_tr.contains(phrase))
    {
        return Some(HintStrength::Phrase);
    }
    let words = lower.split_whitespace().count();
    (speech.is_some_and(|d| d > MACHINE_GREETING_DURATION) && words > MACHINE_GREETING_WORDS)
        .then_some(HintStrength::Length)
}

/// Whether [`voicemail_hint`] finds any hint (a phrase or a long utterance). Use
/// [`voicemail_hint`] to drive the [`VoicemailDriver`]: the strength decides whether a `Human`
/// verdict may be upgraded.
pub fn looks_like_voicemail(text: &str, speech: Option<Duration>) -> bool {
    voicemail_hint(text, speech).is_some()
}

/// Texts and settings of [`voicemail_flow`].
#[derive(Debug, Clone)]
pub struct VoicemailConfig {
    /// The language of the node prompts.
    pub language: Language,
    /// The role of the agent (the system prompt of every node).
    pub role: String,
    /// What the agent does when a person answers (the `human` node's task).
    pub human_task: String,
    /// The fixed message left on a machine. Spoken as it is: put nothing in it the callee did not
    /// consent to hear on a recording.
    pub message: String,
}

impl VoicemailConfig {
    /// A configuration with the default role for `language`.
    pub fn new(language: Language, human_task: &str, message: &str) -> Self {
        let role = match language {
            Language::En => "You are a polite phone agent making an outbound call. Speak briefly \
                             and naturally: this is a phone call."
                .to_owned(),
            Language::Tr => "Giden bir aramayı yapan kibar bir telefon görevlisisin. Kısa ve \
                             doğal konuş: bu bir telefon görüşmesi."
                .to_owned(),
        };
        Self {
            language,
            role,
            human_task: human_task.to_owned(),
            message: message.to_owned(),
        }
    }
}

/// The leave-message flow over `registry` (see the module docs for the nodes).
///
/// When `registry` has an `end_call` tool (see
/// [`register_telephony_tools`](super::register_telephony_tools)) the `human` node offers it and
/// it leads to `farewell`; otherwise the conversation ends by the application's own means.
///
/// # Errors
/// [`FlowDefError`] if a prompt is empty (a blank role, task or message).
pub fn voicemail_flow(
    config: &VoicemailConfig,
    registry: Arc<ToolRegistry>,
) -> Result<Flow, FlowDefError> {
    let (classify, wait, leave, farewell) = match config.language {
        Language::En => (
            "A call was just placed. Wait silently: do not speak until you are told who answered.",
            "An answering machine picked up and its greeting is playing. Stay silent.",
            "An answering machine finished its greeting; the fixed message is being played.",
            "Say a short goodbye.",
        ),
        Language::Tr => (
            "Arama yeni yapıldı. Sessizce bekle: kimin açtığı söylenene kadar konuşma.",
            "Bir telesekreter açtı ve karşılama mesajı çalıyor. Sessiz kal.",
            "Telesekreter karşılama mesajını bitirdi; sabit mesaj çalınıyor.",
            "Kısa bir veda cümlesi söyle.",
        ),
    };
    if config.message.trim().is_empty() {
        return Err(FlowDefError::EmptyPrompt(NodeId::new(NODE_LEAVE_MESSAGE)));
    }
    let has_end_call = registry.spec("end_call").is_some();
    let mut human = Node::new(NODE_HUMAN)
        .task(config.human_task.as_str())
        .with_respond_immediately(true);
    let mut builder = FlowBuilder::new("voicemail").role(config.role.as_str());
    if has_end_call {
        human = human.tool(ToolBinding::new("end_call").goto(NODE_FAREWELL));
    }
    builder = builder
        .node(
            Node::new(NODE_CLASSIFY)
                .task(classify)
                .with_respond_immediately(false),
        )
        .node(human)
        .node(
            Node::new(NODE_WAIT_GREETING)
                .task(wait)
                .with_respond_immediately(false)
                .pre_action(barge_in(false)),
        )
        .node(
            Node::new(NODE_LEAVE_MESSAGE)
                .task(leave)
                .with_strategy(ContextStrategy::Reset)
                .with_respond_immediately(false)
                .pre_action(barge_in(false))
                .pre_action(Action::Say(config.message.as_str().into()))
                .pre_action(Action::EndCall),
        );
    if has_end_call {
        builder = builder.node(
            Node::new(NODE_FAREWELL)
                .task(farewell)
                .post_action(Action::EndCall),
        );
    }
    builder.initial(NODE_CLASSIFY).build(registry)
}

fn barge_in(enabled: bool) -> Action {
    Action::Custom {
        name: ACTION_BARGE_IN.into(),
        payload: json!({ "enabled": enabled }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        #[allow(clippy::disallowed_methods)] // tests need some Instant value
        Instant::now()
    }

    #[test]
    fn human_verdict_enters_the_human_node() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        assert_eq!(d.phase(), Phase::Classifying);
        assert_eq!(d.deadline(), Some(t0 + DEFAULT_CLASSIFY_TIMEOUT));
        assert_eq!(
            d.on_verdict(AnsweredBy::Human, t0),
            Step::Enter(VoicemailNode::Human)
        );
        assert_eq!(
            (d.phase(), d.outcome()),
            (Phase::Human, Some(CallOutcome::Human))
        );
        assert_eq!(d.deadline(), None);
        // Outside the upgrade window a late machine verdict or hint changes nothing.
        let late = t0 + DEFAULT_HUMAN_UPGRADE_WINDOW;
        assert_eq!(d.on_verdict(AnsweredBy::MachineEndBeep, late), Step::Stay);
        assert_eq!(d.on_voicemail_hint(HintStrength::Phrase, late), Step::Stay);
        assert_eq!(d.phase(), Phase::Human);
    }

    #[test]
    fn a_hint_after_a_human_verdict_upgrades_to_machine_within_the_window() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        d.on_verdict(AnsweredBy::Human, t0);
        let t1 = t0 + Duration::from_secs(3);
        assert_eq!(
            d.on_voicemail_hint(HintStrength::Phrase, t1),
            Step::Enter(VoicemailNode::WaitGreeting)
        );
        assert_eq!((d.phase(), d.outcome()), (Phase::WaitingGreeting, None));
        assert_eq!(d.deadline(), Some(t1 + DEFAULT_HINT_WAIT));
        // No event will come: the hint wait ends and the message is spoken.
        assert_eq!(
            d.on_timeout(t1 + DEFAULT_HINT_WAIT),
            Step::Enter(VoicemailNode::LeaveMessage)
        );
    }

    #[test]
    fn a_late_machine_verdict_after_human_upgrades_within_the_window() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        d.on_verdict(AnsweredBy::Human, t0);
        assert_eq!(
            d.on_verdict(AnsweredBy::MachineEndBeep, t0 + Duration::from_secs(4)),
            Step::Enter(VoicemailNode::LeaveMessage)
        );
        assert_eq!(d.outcome(), None);
        // A zero window disables the upgrade.
        let policy = VoicemailPolicy {
            human_upgrade_window: Duration::ZERO,
            ..VoicemailPolicy::default()
        };
        let mut d = VoicemailDriver::new(policy, t0);
        d.on_verdict(AnsweredBy::Human, t0);
        assert_eq!(d.on_voicemail_hint(HintStrength::Phrase, t0), Step::Stay);
        assert_eq!(d.on_verdict(AnsweredBy::MachineEndBeep, t0), Step::Stay);
        // The same for a phrase heard before the verdict.
        let mut d = VoicemailDriver::new(policy, t0);
        d.on_voicemail_hint(HintStrength::Phrase, t0);
        assert_eq!(
            d.on_verdict(AnsweredBy::Human, t0),
            Step::Enter(VoicemailNode::Human)
        );
    }

    #[test]
    fn a_length_hint_never_upgrades_a_human_verdict() {
        // A live person with a long hello: the provider says Human, the STT final is a 4 s,
        // 10-word utterance. Only a phrase may override Human; the length rule must not.
        let long = "Hello, this is the Smith residence, how can I help you";
        let speech = Some(Duration::from_millis(4000));
        let strength = voicemail_hint(long, speech);
        assert_eq!(strength, Some(HintStrength::Length));
        let strength = strength.unwrap();

        // Hint after the verdict.
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        d.on_verdict(AnsweredBy::Human, t0);
        assert_eq!(
            d.on_voicemail_hint(strength, t0 + Duration::from_secs(2)),
            Step::Stay
        );
        assert_eq!(
            (d.phase(), d.outcome(), d.deadline()),
            (Phase::Human, Some(CallOutcome::Human), None)
        );
        // Hint before the verdict.
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        assert_eq!(d.on_voicemail_hint(strength, t0), Step::Stay);
        assert_eq!(
            d.on_verdict(AnsweredBy::Human, t0 + Duration::from_secs(1)),
            Step::Enter(VoicemailNode::Human)
        );
        assert_eq!(d.phase(), Phase::Human);
        // But the same length hint still settles an Unknown verdict as a machine.
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        d.on_voicemail_hint(strength, t0);
        assert_eq!(
            d.on_verdict(AnsweredBy::Unknown, t0),
            Step::Enter(VoicemailNode::WaitGreeting)
        );
    }

    #[test]
    fn a_phrase_heard_before_the_human_verdict_is_not_lost() {
        // Twilio DetectMessageEnd answers `human` for a short greeting that then says "leave a
        // message after the beep"; the STT final may arrive before the verdict.
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        assert_eq!(d.on_voicemail_hint(HintStrength::Phrase, t0), Step::Stay);
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(
            d.on_verdict(AnsweredBy::Human, t1),
            Step::Enter(VoicemailNode::WaitGreeting)
        );
        assert_eq!((d.phase(), d.outcome()), (Phase::WaitingGreeting, None));
        assert_eq!(d.deadline(), Some(t1 + DEFAULT_HINT_WAIT));
        assert_eq!(
            d.on_timeout(t1 + DEFAULT_HINT_WAIT),
            Step::Enter(VoicemailNode::LeaveMessage)
        );
        // A phrase heard long before the verdict (outside the window) is stale.
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        d.on_voicemail_hint(HintStrength::Phrase, t0);
        assert_eq!(
            d.on_verdict(AnsweredBy::Human, t0 + DEFAULT_HUMAN_UPGRADE_WINDOW),
            Step::Enter(VoicemailNode::Human)
        );
    }

    #[test]
    fn a_hint_while_classifying_settles_an_unknown_verdict_as_machine() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        assert_eq!(d.on_voicemail_hint(HintStrength::Phrase, t0), Step::Stay);
        assert_eq!(
            d.on_verdict(AnsweredBy::Unknown, t0),
            Step::Enter(VoicemailNode::WaitGreeting)
        );
        // Without the hint the same verdict means a person.
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        assert_eq!(
            d.on_verdict(AnsweredBy::Unknown, t0),
            Step::Enter(VoicemailNode::Human)
        );
        // A hint never overrides a hang-up policy.
        let policy = VoicemailPolicy {
            on_unknown: Fallback::HangUp,
            ..VoicemailPolicy::default()
        };
        let mut d = VoicemailDriver::new(policy, t0);
        d.on_voicemail_hint(HintStrength::Phrase, t0);
        assert_eq!(d.on_verdict(AnsweredBy::Unknown, t0), Step::HangUp);
    }

    #[test]
    fn detect_only_providers_never_report_a_greeting_end() {
        let t0 = now();
        // Twilio `Enable` / Telnyx `detect`: MachineStart is the last event. Hang up.
        let mut d = VoicemailDriver::new(VoicemailPolicy::detect_only(), t0);
        assert_eq!(d.on_verdict(AnsweredBy::MachineStart, t0), Step::HangUp);
        assert_eq!(d.outcome(), Some(CallOutcome::MachineHangUp));
        // Opting in to speaking waits a fixed time instead of the 30 s greeting timeout.
        let policy = VoicemailPolicy {
            speak_when_greeting_end_uncertain: true,
            ..VoicemailPolicy::detect_only()
        };
        let mut d = VoicemailDriver::new(policy, t0);
        assert_eq!(
            d.on_verdict(AnsweredBy::MachineStart, t0),
            Step::Enter(VoicemailNode::WaitGreeting)
        );
        assert_eq!(d.deadline(), Some(t0 + DEFAULT_HINT_WAIT));
    }

    #[test]
    fn machine_waits_for_the_greeting_end_then_leaves_the_message() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        let t1 = t0 + Duration::from_secs(4);
        assert_eq!(
            d.on_verdict(AnsweredBy::MachineStart, t1),
            Step::Enter(VoicemailNode::WaitGreeting)
        );
        assert_eq!(d.deadline(), Some(t1 + DEFAULT_GREETING_TIMEOUT));
        assert_eq!(d.on_verdict(AnsweredBy::MachineStart, t1), Step::Stay);
        assert_eq!(d.on_verdict(AnsweredBy::Human, t1), Step::Stay);
        assert_eq!(
            d.on_verdict(AnsweredBy::MachineEndBeep, t1 + Duration::from_secs(5)),
            Step::Enter(VoicemailNode::LeaveMessage)
        );
        assert_eq!(d.outcome(), None);
        d.message_played();
        assert_eq!(
            (d.phase(), d.outcome()),
            (Phase::Done, Some(CallOutcome::MachineMessageLeft))
        );
    }

    #[test]
    fn a_direct_message_end_verdict_skips_the_wait() {
        let t0 = now();
        for end in [
            AnsweredBy::MachineEndBeep,
            AnsweredBy::MachineEndSilence,
            AnsweredBy::MachineEndOther,
        ] {
            let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
            assert_eq!(
                d.on_verdict(end, t0),
                Step::Enter(VoicemailNode::LeaveMessage)
            );
            assert_eq!(d.phase(), Phase::Leaving);
        }
    }

    #[test]
    fn fax_hangs_up_and_unknown_follows_the_policy() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        assert_eq!(d.on_verdict(AnsweredBy::Fax, t0), Step::HangUp);
        assert_eq!(d.outcome(), Some(CallOutcome::Fax));

        for (fallback, step) in [
            (Fallback::TreatAsHuman, Step::Enter(VoicemailNode::Human)),
            // Never speaks at once: the greeting may still be playing.
            (
                Fallback::TreatAsMachine,
                Step::Enter(VoicemailNode::WaitGreeting),
            ),
            (Fallback::HangUp, Step::HangUp),
        ] {
            let policy = VoicemailPolicy {
                on_unknown: fallback,
                ..VoicemailPolicy::default()
            };
            let mut d = VoicemailDriver::new(policy, t0);
            assert_eq!(d.on_verdict(AnsweredBy::Unknown, t0), step, "{fallback:?}");
            let mut d = VoicemailDriver::new(policy, t0);
            assert_eq!(
                d.on_timeout(t0 + DEFAULT_CLASSIFY_TIMEOUT),
                step,
                "{fallback:?}"
            );
        }
    }

    #[test]
    fn treat_as_machine_waits_before_speaking_over_a_greeting() {
        let t0 = now();
        let policy = VoicemailPolicy {
            on_unknown: Fallback::TreatAsMachine,
            ..VoicemailPolicy::default()
        };
        // Unknown verdict, and the classify timeout: both wait `hint_wait` first.
        for at in [t0, t0 + DEFAULT_CLASSIFY_TIMEOUT] {
            let mut d = VoicemailDriver::new(policy, t0);
            let step = if at == t0 {
                d.on_verdict(AnsweredBy::Unknown, at)
            } else {
                d.on_timeout(at)
            };
            assert_eq!(step, Step::Enter(VoicemailNode::WaitGreeting));
            assert_eq!(d.deadline(), Some(at + DEFAULT_HINT_WAIT));
            assert_eq!(d.on_timeout(at + Duration::from_secs(5)), Step::Stay);
            assert_eq!(
                d.on_timeout(at + DEFAULT_HINT_WAIT),
                Step::Enter(VoicemailNode::LeaveMessage)
            );
        }
        // A greeting-end verdict while waiting speaks at once.
        let mut d = VoicemailDriver::new(policy, t0);
        d.on_verdict(AnsweredBy::Unknown, t0);
        assert_eq!(
            d.on_verdict(AnsweredBy::MachineEndBeep, t0 + Duration::from_secs(2)),
            Step::Enter(VoicemailNode::LeaveMessage)
        );
        // With speaking when uncertain off, the wait ends in a hang-up.
        let strict = VoicemailPolicy {
            speak_when_greeting_end_uncertain: false,
            ..policy
        };
        let mut d = VoicemailDriver::new(strict, t0);
        d.on_verdict(AnsweredBy::Unknown, t0);
        assert_eq!(d.on_timeout(t0 + DEFAULT_HINT_WAIT), Step::HangUp);
        assert_eq!(d.outcome(), Some(CallOutcome::MachineHangUp));
    }

    #[test]
    fn timeouts_fire_only_at_the_deadline() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        assert_eq!(d.on_timeout(t0 + Duration::from_secs(44)), Step::Stay);
        d.on_verdict(AnsweredBy::MachineStart, t0);
        assert_eq!(d.on_timeout(t0 + Duration::from_secs(29)), Step::Stay);
        assert_eq!(
            d.on_timeout(t0 + DEFAULT_GREETING_TIMEOUT),
            Step::Enter(VoicemailNode::LeaveMessage)
        );

        let strict = VoicemailPolicy {
            speak_when_greeting_end_uncertain: false,
            ..VoicemailPolicy::default()
        };
        let mut d = VoicemailDriver::new(strict, t0);
        d.on_verdict(AnsweredBy::MachineStart, t0);
        assert_eq!(d.on_verdict(AnsweredBy::Unknown, t0), Step::HangUp);
        assert_eq!(d.outcome(), Some(CallOutcome::MachineHangUp));
    }

    #[test]
    fn message_played_only_counts_while_leaving() {
        let t0 = now();
        let mut d = VoicemailDriver::new(VoicemailPolicy::default(), t0);
        d.message_played();
        assert_eq!(d.phase(), Phase::Classifying);
    }

    #[test]
    fn the_hint_spots_voicemail_phrases_and_long_greetings() {
        assert!(looks_like_voicemail(
            "Please leave a message after the beep",
            None
        ));
        assert!(looks_like_voicemail(
            "Bip sesinden sonra mesajınızı bırakın",
            None
        ));
        assert!(looks_like_voicemail("SESLİ MESAJ servisi", None));
        assert!(looks_like_voicemail("MESAJ BIRAKIN", None));
        assert!(looks_like_voicemail(
            "LEAVE A MESSAGE FOR THE VOICEMAIL",
            None
        ));
        assert!(!looks_like_voicemail(
            "Alo? Buyurun",
            Some(Duration::from_millis(900))
        ));
        assert!(!looks_like_voicemail("hello who is this", None));
        let long = "you have reached the home of the Smith family we are out";
        assert!(looks_like_voicemail(
            long,
            Some(Duration::from_millis(4000))
        ));
        assert!(!looks_like_voicemail(
            long,
            Some(Duration::from_millis(3000))
        ));
        // The strength tells a specific phrase from the weak length rule.
        assert_eq!(
            voicemail_hint("Please leave a message", None),
            Some(HintStrength::Phrase)
        );
        assert_eq!(
            voicemail_hint(long, Some(Duration::from_millis(4000))),
            Some(HintStrength::Length)
        );
        assert_eq!(voicemail_hint("hello who is this", None), None);
        assert!(HintStrength::Length < HintStrength::Phrase);
    }
}
