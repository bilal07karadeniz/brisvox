//! Flow definitions: nodes, tool bindings, transitions, actions and the validating builder.
//! Everything here is immutable data once built; the running state lives in
//! [`FlowManager`](super::FlowManager).

use std::cell::OnceCell;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::context::ToolCall;
use crate::tools::ToolRegistry;

/// Name of a node. Unique within a flow.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(Arc<str>);

impl NodeId {
    /// The node called `name`.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self(name.into())
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NodeId {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What happens to the conversation history when a node is entered by a transition.
///
/// In every case message 0 of the context becomes the new node's prompt (see the module docs),
/// so the prompt is always the leading system message that the long-call window keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextStrategy {
    /// Keep the whole history and swap the prompt (Pipecat Flows `APPEND`).
    #[default]
    Append,
    /// Drop the history: the context becomes the new prompt alone (Pipecat Flows `RESET`).
    /// The LLM then sees no user message unless [`Node::with_keep_last_user`] copies the caller's
    /// latest one, so a node with this strategy should be entered with `respond_immediately`,
    /// keep the last user message and carry in its prompt everything it needs (use state
    /// placeholders).
    Reset,
    /// Replace the history by a summary of it (Pipecat Flows `RESET_WITH_SUMMARY`). The summary
    /// comes from a [`Summarizer`](crate::window::Summarizer); when there is none, or it fails or
    /// times out, the transition falls back to [`Self::Append`] (history is never lost silently).
    ResetWithSummary,
}

/// Something the caller of the flow does on entering a node. The flow only names the action; the
/// runner performs it (the same way it plays a tool filler).
///
/// # Action contract
/// - The actions of a list ([`Node::pre_actions`], [`Node::post_actions`]) run **strictly in
///   order**, each one finishing before the next starts.
/// - [`Action::Say`] finishes when its utterance-end mark came back from the playout (the text
///   was played to the caller), or when the runner's own bound on that wait passed. A later
///   action therefore never cuts the sentence.
/// - [`Action::EndCall`] hangs up after everything queued before it was played: the node's
///   response when it is a post action, the preceding `Say`s when it follows them as a pre
///   action (a node with `respond_immediately(false)` that says a fixed message and ends the
///   call, as the voicemail flow does). It never hangs up while earlier speech is still playing.
/// - [`Action::Custom`] is interpreted by the runner; an unknown name is ignored with a warning
///   and does not stop the list.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Speak this fixed text (a canned utterance, TTS only). It is not added to the context.
    Say(Arc<str>),
    /// Hang up the call once everything queued before it was played (see the action contract).
    EndCall,
    /// An application-defined action, interpreted by the runner.
    Custom {
        /// What to do (an identifier the runner understands).
        name: Arc<str>,
        /// Free-form arguments.
        payload: Value,
    },
}

/// A tool result as a transition sees it.
pub struct ToolOutcome<'a> {
    call: &'a ToolCall,
    content: &'a str,
    is_error: bool,
    json: OnceCell<Option<Value>>,
}

impl<'a> ToolOutcome<'a> {
    /// The outcome of `call` with result text `content`.
    pub fn new(call: &'a ToolCall, content: &'a str, is_error: bool) -> Self {
        Self {
            call,
            content,
            is_error,
            json: OnceCell::new(),
        }
    }

    /// The call.
    pub fn call(&self) -> &ToolCall {
        self.call
    }

    /// The tool name.
    pub fn tool(&self) -> &str {
        &self.call.name
    }

    /// The call's arguments (a JSON object).
    pub fn arguments(&self) -> &Value {
        &self.call.arguments
    }

    /// The result text given to the model.
    pub fn content(&self) -> &str {
        self.content
    }

    /// Whether the call failed (engine errors such as `timeout` are failures too).
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// The result parsed as JSON (`None` for text results), parsed at most once.
    pub fn result_json(&self) -> Option<&Value> {
        self.json
            .get_or_init(|| serde_json::from_str(self.content).ok())
            .as_ref()
    }
}

impl fmt::Debug for ToolOutcome<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolOutcome")
            .field("tool", &self.call.name)
            .field("is_error", &self.is_error)
            .finish_non_exhaustive()
    }
}

/// When a [`Rule`] fires. JSON pointers are RFC 6901 (`/available`, `/slots/0`).
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// Always, even for a failed call that ran (`tool_failed`, `timeout`, `unknown_outcome`: a
    /// SideEffect call that timed out may still be running, so prefer [`Self::Success`] for a
    /// transition that must follow a booking). Never for an answer the engine made up without
    /// running the tool (`budget_exhausted`, `stale_turn`, `interrupted`, `duplicate_call`,
    /// `invalid_arguments`, `arguments_too_large`, `too_many_calls`, `unknown_tool`,
    /// `store_full`): those are not tool results and no condition or [`Transition::Custom`]
    /// function sees them.
    Always,
    /// The call succeeded.
    Success,
    /// The call failed (an error result).
    Failure,
    /// The result is JSON and the value at `pointer` equals `value`.
    ResultEquals {
        /// JSON pointer into the result.
        pointer: Arc<str>,
        /// The value it must equal.
        value: Value,
    },
    /// The result is JSON and has a non-null value at `pointer`.
    ResultPresent {
        /// JSON pointer into the result.
        pointer: Arc<str>,
    },
    /// The value at `pointer` of the call's arguments equals `value`.
    ArgEquals {
        /// JSON pointer into the arguments.
        pointer: Arc<str>,
        /// The value it must equal.
        value: Value,
    },
}

impl Condition {
    /// Shorthand for [`Self::ResultEquals`].
    pub fn result_equals(pointer: impl Into<Arc<str>>, value: Value) -> Self {
        Self::ResultEquals {
            pointer: pointer.into(),
            value,
        }
    }

    /// Whether the condition holds for `outcome`.
    pub fn matches(&self, outcome: &ToolOutcome<'_>) -> bool {
        match self {
            Self::Always => true,
            Self::Success => !outcome.is_error(),
            Self::Failure => outcome.is_error(),
            Self::ResultEquals { pointer, value } => outcome
                .result_json()
                .and_then(|json| json.pointer(pointer))
                .is_some_and(|found| found == value),
            Self::ResultPresent { pointer } => outcome
                .result_json()
                .and_then(|json| json.pointer(pointer))
                .is_some_and(|found| !found.is_null()),
            Self::ArgEquals { pointer, value } => outcome
                .arguments()
                .pointer(pointer)
                .is_some_and(|found| found == value),
        }
    }
}

/// One branch of [`Transition::Rules`].
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// When the branch fires.
    pub when: Condition,
    /// The node it leads to.
    pub to: NodeId,
}

impl Rule {
    /// A branch to `to` when `when` holds.
    pub fn new(when: Condition, to: impl Into<NodeId>) -> Self {
        Self {
            when,
            to: to.into(),
        }
    }
}

/// The user-supplied decision function of [`Transition::Custom`].
type Decide = dyn Fn(&ToolOutcome<'_>) -> Option<NodeId> + Send + Sync;

/// What a tool's result does to the flow.
#[derive(Clone)]
pub enum Transition {
    /// Go to this node when the call succeeded (a failed call stays: the model retries).
    Goto(NodeId),
    /// Evaluate the rules in order; the first whose condition holds decides. No match: stay.
    Rules(Vec<Rule>),
    /// A function decides: `Some(node)` transitions, `None` stays. A node that is not in the
    /// flow is rejected at run time (reported, no transition).
    Custom(Arc<Decide>),
}

impl Transition {
    /// A [`Self::Custom`] transition.
    pub fn custom(
        decide: impl Fn(&ToolOutcome<'_>) -> Option<NodeId> + Send + Sync + 'static,
    ) -> Self {
        Self::Custom(Arc::new(decide))
    }

    /// The target for `outcome`, if the transition fires.
    pub fn evaluate(&self, outcome: &ToolOutcome<'_>) -> Option<NodeId> {
        match self {
            Self::Goto(node) => (!outcome.is_error()).then(|| node.clone()),
            Self::Rules(rules) => rules
                .iter()
                .find(|rule| rule.when.matches(outcome))
                .map(|rule| rule.to.clone()),
            Self::Custom(decide) => decide(outcome),
        }
    }

    /// The statically known targets (none for a custom function).
    fn targets(&self) -> Vec<&NodeId> {
        match self {
            Self::Goto(node) => vec![node],
            Self::Rules(rules) => rules.iter().map(|rule| &rule.to).collect(),
            Self::Custom(_) => Vec::new(),
        }
    }
}

impl fmt::Debug for Transition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Goto(node) => f.debug_tuple("Goto").field(node).finish(),
            Self::Rules(rules) => f.debug_tuple("Rules").field(rules).finish(),
            Self::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

/// A tool as one node offers it: the registry tool by name, what its result does to the flow, and
/// what of it is remembered in the flow state.
#[derive(Debug, Clone)]
pub struct ToolBinding {
    pub(crate) name: Arc<str>,
    pub(crate) transition: Option<Transition>,
    pub(crate) capture_args: bool,
    pub(crate) capture_results: Vec<Arc<str>>,
}

impl ToolBinding {
    /// The registry tool `name`, with no transition and nothing captured.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            transition: None,
            capture_args: false,
            capture_results: Vec::new(),
        }
    }

    /// The tool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Transition to `node` when a call succeeds (an "explicit transition function").
    #[must_use]
    pub fn goto(mut self, node: impl Into<NodeId>) -> Self {
        self.transition = Some(Transition::Goto(node.into()));
        self
    }

    /// Transition by [`Rule`]s.
    #[must_use]
    pub fn rules(mut self, rules: Vec<Rule>) -> Self {
        self.transition = Some(Transition::Rules(rules));
        self
    }

    /// Transition by a function.
    #[must_use]
    pub fn custom(
        mut self,
        decide: impl Fn(&ToolOutcome<'_>) -> Option<NodeId> + Send + Sync + 'static,
    ) -> Self {
        self.transition = Some(Transition::custom(decide));
        self
    }

    /// Merge the top-level arguments of every successful call into the flow state.
    #[must_use]
    pub fn capture_args(mut self) -> Self {
        self.capture_args = true;
        self
    }

    /// Store the result of every successful call in the flow state under `key` (JSON when it
    /// parses, otherwise the text).
    #[must_use]
    pub fn capture_result(mut self, key: impl Into<Arc<str>>) -> Self {
        self.capture_results.push(key.into());
        self
    }
}

/// One step of the conversation: a prompt, the tools offered, and how it is entered.
#[derive(Debug, Clone)]
pub struct Node {
    pub(crate) id: NodeId,
    pub(crate) role: Option<Vec<Arc<str>>>,
    pub(crate) task: Vec<Arc<str>>,
    pub(crate) tools: Vec<ToolBinding>,
    pub(crate) strategy: ContextStrategy,
    pub(crate) pre_actions: Vec<Action>,
    pub(crate) post_actions: Vec<Action>,
    pub(crate) respond_immediately: bool,
    pub(crate) keep_last_user: bool,
}

impl Node {
    /// A node called `id` with no prompt yet, [`ContextStrategy::Append`] and
    /// `respond_immediately` on.
    pub fn new(id: impl Into<NodeId>) -> Self {
        Self {
            id: id.into(),
            role: None,
            task: Vec::new(),
            tools: Vec::new(),
            strategy: ContextStrategy::Append,
            pre_actions: Vec::new(),
            post_actions: Vec::new(),
            respond_immediately: true,
            keep_last_user: false,
        }
    }

    /// The node's name.
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// The strategy applied when a transition enters this node.
    pub fn strategy(&self) -> ContextStrategy {
        self.strategy
    }

    /// Actions to perform on entering, before the model answers.
    pub fn pre_actions(&self) -> &[Action] {
        &self.pre_actions
    }

    /// Actions to perform after the node's first response was played.
    pub fn post_actions(&self) -> &[Action] {
        &self.post_actions
    }

    /// Whether the model answers right after entering (otherwise the node waits for the caller).
    pub fn respond_immediately(&self) -> bool {
        self.respond_immediately
    }

    /// The tools this node offers (not counting the flow's global tools).
    pub fn tools(&self) -> &[ToolBinding] {
        &self.tools
    }

    /// Adds a role message that replaces the flow's role messages for this node.
    #[must_use]
    pub fn role(mut self, message: impl Into<Arc<str>>) -> Self {
        self.role.get_or_insert_with(Vec::new).push(message.into());
        self
    }

    /// Adds a task message. `{key}` placeholders are filled from the flow state.
    #[must_use]
    pub fn task(mut self, message: impl Into<Arc<str>>) -> Self {
        self.task.push(message.into());
        self
    }

    /// Offers a tool.
    #[must_use]
    pub fn tool(mut self, binding: ToolBinding) -> Self {
        self.tools.push(binding);
        self
    }

    /// Sets the context strategy.
    #[must_use]
    pub fn with_strategy(mut self, strategy: ContextStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Adds a pre action.
    #[must_use]
    pub fn pre_action(mut self, action: Action) -> Self {
        self.pre_actions.push(action);
        self
    }

    /// Adds a post action.
    #[must_use]
    pub fn post_action(mut self, action: Action) -> Self {
        self.post_actions.push(action);
        self
    }

    /// Sets whether the model answers right after entering.
    #[must_use]
    pub fn with_respond_immediately(mut self, respond: bool) -> Self {
        self.respond_immediately = respond;
        self
    }

    /// Whether `Reset` and `ResetWithSummary` keep the caller's latest message: the newest user
    /// message of the history is copied behind the prompt (and the summary), so the model has
    /// something to answer. Off by default. Turn it on for a node that responds immediately: a
    /// context of system messages only is rejected by some providers (Anthropic). Ignored by
    /// `Append`, which keeps the whole history anyway.
    #[must_use]
    pub fn with_keep_last_user(mut self, keep: bool) -> Self {
        self.keep_last_user = keep;
        self
    }

    /// Whether the reset strategies keep the caller's latest message (see
    /// [`Self::with_keep_last_user`]).
    pub fn keep_last_user(&self) -> bool {
        self.keep_last_user
    }
}

/// Limits of a flow's summarization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowConfig {
    /// Size asked of the summarizer, in estimated tokens; a longer summary is clipped.
    pub summary_max_tokens: usize,
    /// How long a `ResetWithSummary` transition waits for the summarizer (injected clock) before
    /// it falls back to `Append`.
    pub summary_timeout: Duration,
    /// Most estimated tokens of history given to the summarizer: whole turns, newest first (the
    /// latest turn always goes in).
    pub summary_input_tokens: usize,
}

impl Default for FlowConfig {
    /// 300 summary tokens, 8 s, 6000 input tokens.
    fn default() -> Self {
        Self {
            summary_max_tokens: 300,
            summary_timeout: Duration::from_secs(8),
            summary_input_tokens: 6000,
        }
    }
}

/// Why a flow definition is rejected by [`FlowBuilder::build`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FlowDefError {
    /// The flow has no node.
    #[error("the flow has no nodes")]
    NoNodes,
    /// Two nodes share a name.
    #[error("duplicate node {0}")]
    DuplicateNode(NodeId),
    /// The initial node is not in the flow.
    #[error("the initial node {0} does not exist")]
    UnknownInitial(NodeId),
    /// A node has no prompt text (neither role nor task messages).
    #[error("node {0} has an empty prompt")]
    EmptyPrompt(NodeId),
    /// A node offers a tool twice.
    #[error("node {node} offers tool {tool:?} twice")]
    DuplicateTool {
        /// The node (`<global>` for the flow's global tools).
        node: NodeId,
        /// The tool.
        tool: Arc<str>,
    },
    /// A node offers a tool that is not in the registry.
    #[error("node {node} offers tool {tool:?}, which is not registered")]
    UnknownTool {
        /// The node (`<global>` for the flow's global tools).
        node: NodeId,
        /// The tool.
        tool: Arc<str>,
    },
    /// A transition leads to a node that is not in the flow.
    #[error("node {node}: tool {tool:?} transitions to unknown node {target}")]
    UnknownTarget {
        /// The node (`<global>` for the flow's global tools).
        node: NodeId,
        /// The tool.
        tool: Arc<str>,
        /// The missing node.
        target: NodeId,
    },
}

/// Name used in [`FlowDefError`] for the flow's global tools.
const GLOBAL: &str = "<global>";

/// Builds a [`Flow`].
#[derive(Debug)]
pub struct FlowBuilder {
    name: Arc<str>,
    role: Vec<Arc<str>>,
    nodes: Vec<Node>,
    initial: Option<NodeId>,
    global_tools: Vec<ToolBinding>,
    config: FlowConfig,
}

impl FlowBuilder {
    /// A builder for a flow called `name` (used in logs).
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            role: Vec::new(),
            nodes: Vec::new(),
            initial: None,
            global_tools: Vec::new(),
            config: FlowConfig::default(),
        }
    }

    /// Adds a role message shared by all nodes that do not set their own (the persona).
    #[must_use]
    pub fn role(mut self, message: impl Into<Arc<str>>) -> Self {
        self.role.push(message.into());
        self
    }

    /// Adds a node. The first node added is the initial one unless [`Self::initial`] says
    /// otherwise.
    #[must_use]
    pub fn node(mut self, node: Node) -> Self {
        self.nodes.push(node);
        self
    }

    /// Sets the initial node.
    #[must_use]
    pub fn initial(mut self, node: impl Into<NodeId>) -> Self {
        self.initial = Some(node.into());
        self
    }

    /// Offers a tool in every node. A node's own binding of the same name takes precedence.
    #[must_use]
    pub fn global_tool(mut self, binding: ToolBinding) -> Self {
        self.global_tools.push(binding);
        self
    }

    /// Sets the summarization limits.
    #[must_use]
    pub fn config(mut self, config: FlowConfig) -> Self {
        self.config = config;
        self
    }

    /// Validates the definition against `registry` (the registry the tool engine runs).
    ///
    /// # Errors
    /// [`FlowDefError`] for a flow without nodes, duplicate nodes, an unknown initial node, a node
    /// without prompt text, a duplicate or unregistered tool, or a static transition target that
    /// is not a node.
    pub fn build(self, registry: Arc<ToolRegistry>) -> Result<Flow, FlowDefError> {
        let Self {
            name,
            role,
            nodes,
            initial,
            global_tools,
            config,
        } = self;
        let mut map: HashMap<NodeId, Arc<Node>> = HashMap::with_capacity(nodes.len());
        let mut order = Vec::with_capacity(nodes.len());
        for node in nodes {
            if map.contains_key(&node.id) {
                return Err(FlowDefError::DuplicateNode(node.id));
            }
            order.push(node.id.clone());
            map.insert(node.id.clone(), Arc::new(node));
        }
        let first = order.first().ok_or(FlowDefError::NoNodes)?;
        let initial_id = initial.unwrap_or_else(|| first.clone());
        let initial = map
            .get(&initial_id)
            .cloned()
            .ok_or(FlowDefError::UnknownInitial(initial_id))?;

        let global_scope = NodeId::new(GLOBAL);
        check_bindings(&global_scope, &global_tools, &map, &registry)?;
        for node in map.values() {
            check_bindings(&node.id, &node.tools, &map, &registry)?;
            let has_text = |messages: &[Arc<str>]| messages.iter().any(|m| !m.trim().is_empty());
            let role_text = node.role.as_deref().unwrap_or(&role);
            if !has_text(role_text) && !has_text(&node.task) {
                return Err(FlowDefError::EmptyPrompt(node.id.clone()));
            }
        }
        Ok(Flow {
            name,
            role,
            nodes: map,
            order,
            initial,
            global_tools,
            config,
            registry,
        })
    }
}

/// Checks one scope's bindings: unique names, registered tools, existing static targets.
fn check_bindings(
    scope: &NodeId,
    bindings: &[ToolBinding],
    nodes: &HashMap<NodeId, Arc<Node>>,
    registry: &ToolRegistry,
) -> Result<(), FlowDefError> {
    let mut seen = BTreeSet::new();
    for binding in bindings {
        if !seen.insert(binding.name.clone()) {
            return Err(FlowDefError::DuplicateTool {
                node: scope.clone(),
                tool: binding.name.clone(),
            });
        }
        if registry.spec(&binding.name).is_none() {
            return Err(FlowDefError::UnknownTool {
                node: scope.clone(),
                tool: binding.name.clone(),
            });
        }
        if let Some(target) = binding
            .transition
            .iter()
            .flat_map(Transition::targets)
            .find(|target| !nodes.contains_key(*target))
        {
            return Err(FlowDefError::UnknownTarget {
                node: scope.clone(),
                tool: binding.name.clone(),
                target: target.clone(),
            });
        }
    }
    Ok(())
}

/// A validated conversation flow: named nodes over a tool registry. Immutable and shareable
/// (`Arc<Flow>`); every call runs its own [`FlowManager`](super::FlowManager) over it.
pub struct Flow {
    pub(crate) name: Arc<str>,
    pub(crate) role: Vec<Arc<str>>,
    pub(crate) nodes: HashMap<NodeId, Arc<Node>>,
    order: Vec<NodeId>,
    pub(crate) initial: Arc<Node>,
    pub(crate) global_tools: Vec<ToolBinding>,
    pub(crate) config: FlowConfig,
    pub(crate) registry: Arc<ToolRegistry>,
}

impl Flow {
    /// The flow's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The node called `id`.
    pub fn node(&self, id: &NodeId) -> Option<&Arc<Node>> {
        self.nodes.get(id)
    }

    /// The nodes in the order they were defined.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> + '_ {
        self.order
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .map(AsRef::as_ref)
    }

    /// The node a call starts in.
    pub fn initial(&self) -> &NodeId {
        &self.initial.id
    }

    /// The registry the flow was validated against. Give it to the
    /// [`ToolEngine`](crate::tools::ToolEngine) that runs the flow's tools.
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// The summarization limits.
    pub fn config(&self) -> &FlowConfig {
        &self.config
    }

    /// The binding of tool `name` in `node`: the node's own, else a global one.
    pub(crate) fn binding<'a>(&'a self, node: &'a Node, name: &str) -> Option<&'a ToolBinding> {
        node.tools
            .iter()
            .chain(&self.global_tools)
            .find(|binding| &*binding.name == name)
    }
}

impl fmt::Debug for Flow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Flow")
            .field("name", &self.name)
            .field("nodes", &self.order)
            .field("initial", &self.initial.id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ToolCallId;
    use crate::tools::{Tool, ToolCallCtx, ToolFuture, ToolKind, ToolOutput, ToolSpec};
    use brisvox_core::TurnEpoch;
    use serde_json::json;

    struct Echo;
    impl Tool for Echo {
        fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolOutput::Json(args)) })
        }
    }

    fn registry() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        for name in ["a", "b"] {
            registry
                .register(
                    ToolSpec::new(name, "t", json!({"type": "object"}), ToolKind::ReadOnly),
                    Echo,
                )
                .unwrap();
        }
        Arc::new(registry)
    }

    fn node(id: &str) -> Node {
        Node::new(id).task(format!("task of {id}"))
    }

    fn outcome_of<'a>(call: &'a ToolCall, content: &'a str, is_error: bool) -> ToolOutcome<'a> {
        ToolOutcome::new(call, content, is_error)
    }

    fn call(args: Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(TurnEpoch::new(1), 0),
            provider_id: None,
            name: Arc::from("a"),
            arguments: args,
            provider_data: None,
        }
    }

    #[test]
    fn a_valid_flow_builds_and_the_first_node_is_initial() {
        let flow = FlowBuilder::new("f")
            .node(node("one").tool(ToolBinding::new("a").goto("two")))
            .node(node("two"))
            .build(registry())
            .unwrap();
        assert_eq!(flow.initial().as_str(), "one");
        assert_eq!(flow.nodes().count(), 2);
        let explicit = FlowBuilder::new("f")
            .node(node("one"))
            .node(node("two"))
            .initial("two")
            .build(registry())
            .unwrap();
        assert_eq!(explicit.initial().as_str(), "two");
    }

    #[test]
    fn invalid_definitions_are_rejected() {
        let reg = registry;
        assert_eq!(
            FlowBuilder::new("f").build(reg()).unwrap_err(),
            FlowDefError::NoNodes
        );
        assert_eq!(
            FlowBuilder::new("f")
                .node(node("x"))
                .node(node("x"))
                .build(reg())
                .unwrap_err(),
            FlowDefError::DuplicateNode(NodeId::new("x"))
        );
        assert_eq!(
            FlowBuilder::new("f")
                .node(node("x"))
                .initial("y")
                .build(reg())
                .unwrap_err(),
            FlowDefError::UnknownInitial(NodeId::new("y"))
        );
        assert_eq!(
            FlowBuilder::new("f")
                .node(Node::new("x").task("  "))
                .build(reg())
                .unwrap_err(),
            FlowDefError::EmptyPrompt(NodeId::new("x"))
        );
        assert!(matches!(
            FlowBuilder::new("f")
                .node(node("x").tool(ToolBinding::new("zzz")))
                .build(reg())
                .unwrap_err(),
            FlowDefError::UnknownTool { .. }
        ));
        assert!(matches!(
            FlowBuilder::new("f")
                .node(
                    node("x")
                        .tool(ToolBinding::new("a"))
                        .tool(ToolBinding::new("a"))
                )
                .build(reg())
                .unwrap_err(),
            FlowDefError::DuplicateTool { .. }
        ));
        assert!(matches!(
            FlowBuilder::new("f")
                .node(node("x").tool(ToolBinding::new("a").goto("nowhere")))
                .build(reg())
                .unwrap_err(),
            FlowDefError::UnknownTarget { .. }
        ));
        assert!(matches!(
            FlowBuilder::new("f")
                .node(node("x"))
                .global_tool(ToolBinding::new("a").rules(vec![Rule::new(Condition::Always, "nowhere")]))
                .build(reg())
                .unwrap_err(),
            FlowDefError::UnknownTarget { node, .. } if node.as_str() == "<global>"
        ));
    }

    #[test]
    fn a_role_alone_is_a_prompt_and_a_node_role_overrides_it() {
        let flow = FlowBuilder::new("f")
            .role("persona")
            .node(Node::new("x"))
            .build(registry())
            .unwrap();
        assert_eq!(flow.nodes().count(), 1);
        // The flow role is blank, the node brings its own role text.
        assert!(
            FlowBuilder::new("f")
                .node(Node::new("x").role("own"))
                .build(registry())
                .is_ok()
        );
    }

    #[test]
    fn goto_fires_on_success_only_and_rules_pick_the_first_match() {
        let c = call(json!({"n": 2}));
        let ok = outcome_of(&c, r#"{"status":"yes","n":null}"#, false);
        let bad = outcome_of(&c, r#"{"error":"timeout"}"#, true);
        let goto = Transition::Goto(NodeId::new("t"));
        assert_eq!(goto.evaluate(&ok), Some(NodeId::new("t")));
        assert_eq!(goto.evaluate(&bad), None);

        let rules = Transition::Rules(vec![
            Rule::new(Condition::result_equals("/status", json!("no")), "no"),
            Rule::new(Condition::result_equals("/status", json!("yes")), "yes1"),
            Rule::new(
                Condition::ResultPresent {
                    pointer: "/status".into(),
                },
                "yes2",
            ),
            Rule::new(
                Condition::ArgEquals {
                    pointer: "/n".into(),
                    value: json!(2),
                },
                "arg",
            ),
            Rule::new(Condition::Failure, "fail"),
        ]);
        assert_eq!(rules.evaluate(&ok), Some(NodeId::new("yes1")));
        // The error body has no /status; it falls through to the failure rule, after ArgEquals.
        assert_eq!(rules.evaluate(&bad), Some(NodeId::new("arg")));
        let absent = outcome_of(&c, r#"{"n":null}"#, false);
        let only_present = Transition::Rules(vec![Rule::new(
            Condition::ResultPresent {
                pointer: "/n".into(),
            },
            "p",
        )]);
        assert_eq!(only_present.evaluate(&absent), None, "null is not present");
        let text = outcome_of(&c, "plain text", false);
        assert_eq!(rules.evaluate(&text), Some(NodeId::new("arg")));
        assert!(Condition::Success.matches(&text));
        assert!(Condition::Always.matches(&bad));
    }

    #[test]
    fn a_custom_transition_sees_the_outcome() {
        let t = Transition::custom(|o| {
            (o.tool() == "a" && o.result_json().is_some()).then(|| NodeId::new("seen"))
        });
        let c = call(json!({}));
        assert_eq!(
            t.evaluate(&outcome_of(&c, "{}", false)),
            Some(NodeId::new("seen"))
        );
        assert_eq!(t.evaluate(&outcome_of(&c, "x", false)), None);
        assert_eq!(format!("{t:?}"), "Custom(..)");
    }
}
