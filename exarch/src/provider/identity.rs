//! Provider identity, subscription flavour, and wire-adapter selection.

use super::oauth;
use clap::ValueEnum;
use genai::adapter::AdapterKind;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProviderKind {
    Anthropic,
    Openai,
    Openrouter,
    Deepseek,
    Gemini,
    OpencodeZen,
    OpencodeGo,
    Xai,
    Qwen,
}

impl ProviderKind {
    /// `(label, default_model, key_env)` for this provider.
    pub fn info(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Anthropic => ("anthropic", "claude-opus-4", "ANTHROPIC_API_KEY"),
            Self::Openai => ("openai", "gpt-5.5", "OPENAI_API_KEY"),
            Self::Openrouter => (
                "openrouter",
                "anthropic/claude-opus-4",
                "OPENROUTER_API_KEY",
            ),
            Self::Deepseek => ("deepseek", "deepseek-chat", "DEEPSEEK_API_KEY"),
            Self::Gemini => ("gemini", "gemini-2.5-pro", "GEMINI_API_KEY"),
            // opencode issues one key per account and tells Zen from Go apart
            // by endpoint, so both read the same `OPENCODE_API_KEY`.
            Self::OpencodeZen => ("opencode-zen", "glm-5.1", "OPENCODE_API_KEY"),
            Self::OpencodeGo => ("opencode-go", "glm-5.2", "OPENCODE_API_KEY"),
            Self::Xai => ("xai", "grok-4.3", "XAI_API_KEY"),
            Self::Qwen => ("qwen", "qwen3.6-plus", "DASHSCOPE_API_KEY"),
        }
    }

    /// Base URL for providers that need a custom endpoint.
    ///
    /// Must end with `/`: genai joins the service path onto it.
    pub fn endpoint(self) -> Option<&'static str> {
        match self {
            Self::Openrouter => Some("https://openrouter.ai/api/v1/"),
            Self::OpencodeZen => Some("https://opencode.ai/zen/v1/"),
            Self::OpencodeGo => Some("https://opencode.ai/zen/go/v1/"),
            Self::Qwen => Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/"),
            _ => None,
        }
    }

    /// The genai adapter this provider uses by default.
    pub fn default_adapter(self) -> AdapterKind {
        match self {
            Self::Anthropic => AdapterKind::Anthropic,
            Self::Openai => AdapterKind::OpenAIResp,
            Self::Openrouter | Self::OpencodeZen | Self::OpencodeGo | Self::Qwen => {
                AdapterKind::OpenAI
            }
            Self::Deepseek => AdapterKind::DeepSeek,
            Self::Gemini => AdapterKind::Gemini,
            Self::Xai => AdapterKind::Xai,
        }
    }

    /// Whether this provider bills as a flat subscription rather than per token.
    pub fn flat_rate(self) -> bool {
        matches!(self, Self::OpencodeGo)
    }
}

/// An unusual provider declared in `config.ral`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomProvider {
    /// The provider's display label and stable identity.
    pub label: String,
    /// Its API-key environment variable, or `None` for a no-auth endpoint.
    pub key_env: Option<String>,
    /// The base URL genai points traffic at.
    pub endpoint: String,
    /// The configured wire protocol.
    pub adapter: AdapterKind,
}

/// A signed-in `ChatGPT` account, as a selectable provider identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGptAccount {
    /// The `OpenAI` account id this selection maps to.
    pub account_id: String,
    /// The account's stable, human-readable handle.
    pub label: String,
}

impl ChatGptAccount {
    /// Make the selectable identity for a stored login.
    pub fn from_token(token: &oauth::OAuthToken) -> Self {
        Self {
            account_id: token.account_id.clone(),
            label: token.label(),
        }
    }
}

/// A famous provider, custom endpoint, or signed-in `ChatGPT` account.
///
/// Equality, ordering, and hashing use the label alone: it is the unique key
/// shared by credential resolution, model listing, and selection.
#[derive(Clone, Debug)]
pub enum ProviderId {
    /// A built-in provider.
    Famous(ProviderKind),
    /// An unusual provider declared in `config.ral`.
    Custom(Arc<CustomProvider>),
    /// A signed-in `ChatGPT` account.
    ChatGpt(Arc<ChatGptAccount>),
}

impl ProviderId {
    /// The stable provider label.
    pub fn label(&self) -> &str {
        match self {
            Self::Famous(kind) => kind.info().0,
            Self::Custom(custom) => &custom.label,
            Self::ChatGpt(account) => &account.label,
        }
    }

    /// The API-key environment variable, if this provider uses one.
    pub fn key_env(&self) -> Option<&str> {
        match self {
            Self::Famous(kind) => Some(kind.info().2),
            Self::Custom(custom) => custom.key_env.as_deref(),
            Self::ChatGpt(_) => None,
        }
    }

    /// The custom endpoint, if this provider has one.
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Famous(kind) => kind.endpoint(),
            Self::Custom(custom) => Some(&custom.endpoint),
            Self::ChatGpt(_) => None,
        }
    }

    /// The provider's default wire adapter.
    pub fn default_adapter(&self) -> AdapterKind {
        match self {
            Self::Famous(kind) => kind.default_adapter(),
            Self::Custom(custom) => custom.adapter,
            Self::ChatGpt(_) => AdapterKind::OpenAIResp,
        }
    }

    /// Whether the provider itself declares a flat-rate plan.
    pub fn flat_rate(&self) -> bool {
        match self {
            Self::Famous(kind) => kind.flat_rate(),
            Self::Custom(_) | Self::ChatGpt(_) => false,
        }
    }

    /// The built-in kind, when this is a famous provider.
    pub fn famous(&self) -> Option<ProviderKind> {
        match self {
            Self::Famous(kind) => Some(*kind),
            Self::Custom(_) | Self::ChatGpt(_) => None,
        }
    }
}

impl PartialEq for ProviderId {
    fn eq(&self, other: &Self) -> bool {
        self.label() == other.label()
    }
}

impl Eq for ProviderId {}

impl std::hash::Hash for ProviderId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.label().hash(state);
    }
}

impl PartialOrd for ProviderId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ProviderId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.label().cmp(other.label())
    }
}

/// How a provider's plan reads, for both metering and labelling.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Subscription {
    /// A metered API key.
    Metered,
    /// A `ChatGPT` plan login authorised over OAuth.
    ChatGpt,
    /// A flat-rate plan declared by the provider.
    FlatRate,
}

/// Decorate a provider label with its subscription flavour.
pub(crate) fn provider_label(subscription: Subscription, base: &str) -> String {
    match subscription {
        Subscription::Metered => base.to_string(),
        Subscription::ChatGpt => format!("{base} (ChatGPT subscription)"),
        Subscription::FlatRate => format!("{base} (subscription)"),
    }
}

pub(super) fn adapter_for_provider_model(id: &ProviderId, model: &str) -> AdapterKind {
    match id {
        ProviderId::Famous(ProviderKind::Openai) => {
            match AdapterKind::from_model(model).unwrap_or(AdapterKind::OpenAIResp) {
                adapter @ (AdapterKind::OpenAI | AdapterKind::OpenAIResp) => adapter,
                _ => AdapterKind::OpenAIResp,
            }
        }
        _ => id.default_adapter(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_provider_keeps_openai_adapter_split() {
        let id = ProviderId::Famous(ProviderKind::Openai);
        assert_eq!(
            adapter_for_provider_model(&id, "gpt-4.1"),
            AdapterKind::OpenAI
        );
        assert_eq!(
            adapter_for_provider_model(&id, "gpt-5.5"),
            AdapterKind::OpenAIResp
        );
    }

    #[test]
    fn custom_provider_id_reports_declared_facts() {
        let id = ProviderId::Custom(Arc::new(CustomProvider {
            label: "local-llama".into(),
            key_env: Some("LOCAL_LLAMA_KEY".into()),
            endpoint: "https://llama.example/v1/".into(),
            adapter: AdapterKind::OpenAI,
        }));
        assert_eq!(id.label(), "local-llama");
        assert_eq!(id.key_env(), Some("LOCAL_LLAMA_KEY"));
        assert_eq!(id.endpoint(), Some("https://llama.example/v1/"));
        assert_eq!(id.default_adapter(), AdapterKind::OpenAI);
        assert_eq!(id.famous(), None);
        assert_eq!(
            adapter_for_provider_model(&id, "gpt-5.5"),
            AdapterKind::OpenAI
        );
    }

    #[test]
    fn provider_label_decorates_per_flavour() {
        assert_eq!(
            provider_label(Subscription::Metered, "deepseek"),
            "deepseek"
        );
        assert_eq!(
            provider_label(Subscription::ChatGpt, "openai"),
            "openai (ChatGPT subscription)"
        );
        assert_eq!(
            provider_label(Subscription::FlatRate, "opencode-go"),
            "opencode-go (subscription)"
        );
    }
}
