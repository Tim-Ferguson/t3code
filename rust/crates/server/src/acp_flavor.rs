//! The driver flavours of the ACP adapter: plain registry agents, and Grok with its extensions. The adapter
//! asks the flavour only for what differs, so the registry path is the unflavoured one.
use crate::{acp_tools::ToolState, antigravity_acp as agy, antigravity_flavor::Antigravity, grok_flavor::Grok, provider_registry::ProviderRegistry, xai_extension as xai};
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) enum Flavor {
    Registry,
    Grok(Grok),
    Antigravity(Antigravity),
}
impl Flavor {
    /// The flavour a thread's provider instance runs as, from the thread's runtime mode.
    pub(crate) fn of(providers: &ProviderRegistry, instance_id: &str, runtime_mode: &str) -> Result<Self, crate::provider_process::ProcessError> {
        if providers.driver(instance_id)? == "grok" {
            return Ok(Self::Grok(Grok::new(providers.grok(instance_id)?, runtime_mode)));
        }
        if providers.driver(instance_id)? == "antigravity" {
            return Ok(Self::Antigravity(Antigravity::new(providers.antigravity(instance_id)?, providers.clone())));
        }
        Ok(Self::Registry)
    }
    /// The provider driver the adapter's rows are written under.
    pub(crate) fn driver(&self) -> &'static str {
        match self {
            Self::Registry => "acpRegistry",
            Self::Grok(_) => "grok",
            Self::Antigravity(_) => "antigravity",
        }
    }
    /// The harness the runtime instructions name.
    pub(crate) fn harness(&self) -> &'static str {
        match self {
            Self::Registry => "acpRegistry",
            Self::Grok(_) => "Grok",
            Self::Antigravity(_) => "Antigravity",
        }
    }
    /// The adapter's default capabilities, before the session narrows them.
    pub(crate) fn capabilities(&self) -> Value {
        serde_json::from_str(match self {
            Self::Registry => include_str!("acp-capabilities.json"),
            Self::Grok(_) => include_str!("grok-capabilities.json"),
            Self::Antigravity(_) => include_str!("antigravity-capabilities.json"),
        })
        .expect("static provider capabilities")
    }
    /// `normalizeToolCall`: Grok rewrites generic titles and keeps start acknowledgements running.
    pub(crate) fn normalize_tool(&self, mut tool: ToolState) -> ToolState {
        match self {
            Self::Grok(_) => tool.wire = xai::normalize_tool_call(&tool.wire),
            Self::Antigravity(_) => tool.wire = agy::normalize_tool_call(&tool.wire),
            Self::Registry => {}
        }
        tool
    }
    /// Grok writes its plan to a file mid-turn; the markdown a tool call carries shows as the proposed-plan card.
    pub(crate) fn proposed_plan(&self, tool: &ToolState, environment: &std::collections::HashMap<String, String>) -> Option<String> {
        let Self::Grok(_) = self else { return None };
        let lookup = |name: &str| environment.get(name).cloned().or_else(|| std::env::var(name).ok());
        let home = std::env::var("HOME").ok();
        xai::plan_markdown_from_tool_data(&tool.wire["data"], cfg!(windows), &lookup, home.as_deref())
    }
    /// The approval choices a permission request offers, when the flavour narrows them.
    pub(crate) fn approval_options(&self, request: &Value) -> Option<Value> {
        match self {
            Self::Registry => None,
            Self::Grok(_) => Some(json!(crate::grok_acp::approval_options(request))),
            Self::Antigravity(_) => Some(json!(agy::approval_options(request))),
        }
    }
    /// A Grok image prompt is accepted though `initialize` reports `promptCapabilities.image: false`.
    pub(crate) fn sends_images_regardless(&self) -> bool {
        matches!(self, Self::Grok(_) | Self::Antigravity(_))
    }
    /// Whether a usage or title update republishes the provider thread with the status the adapter cached.
    pub(crate) fn republishes_cached_thread(&self) -> bool {
        matches!(self, Self::Grok(_))
    }
    pub(crate) fn prompt_failure(&self, agent_id: &str, cause: &t3_acp::AcpError) -> Value {
        match self {
            Self::Registry => crate::provider_failure::acp_prompt(agent_id, cause),
            // No failure interpretation of its own: the generic message, with the cause kept out of the wire.
            Self::Antigravity(_) => crate::provider_failure::make(&json!({"class":"provider_error"})),
            Self::Grok(_) => crate::grok_flavor::prompt_failure(cause),
        }
    }
    /// A permission request that is really a question (Antigravity's `interaction_*` tool calls): the question
    /// the user is asked. Anything else goes through the approval card.
    pub(crate) fn permission_question(&self, request: &Value) -> Option<Value> {
        match self {
            Self::Antigravity(_) => agy::user_input_question(request),
            _ => None,
        }
    }
    /// The answer to a `permission_question`, or `None` for one that is not among its choices.
    pub(crate) fn permission_question_response(&self, request: &Value, answers: &Value) -> Option<Value> {
        match self {
            Self::Antigravity(_) => agy::user_input_response(request, answers),
            _ => None,
        }
    }
    /// The subagent a tool call stands for, when this agent describes subagents as tool calls.
    pub(crate) fn subagent_update(&self, tool: &Value) -> Option<xai::SubagentUpdate> {
        match self {
            Self::Antigravity(_) => agy::subagent_update(tool),
            _ => None,
        }
    }
    /// Whether a batch of subagents without completion signals goes idle when its parent turn ends.
    pub(crate) fn subagents_idle_on_turn_completion(&self) -> bool {
        matches!(self, Self::Antigravity(_))
    }
}
