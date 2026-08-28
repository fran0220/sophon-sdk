use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use http::{HeaderName, HeaderValue};

use crate::{ClientHandler, Error};

/// Provider wire protocol used for model inference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderProtocol {
    #[default]
    OpenAiChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

/// Explicit endpoint, credential, and routing for one model.
///
/// Credentials and custom header/query values are omitted from `Debug` output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderConfig {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub headers: BTreeMap<String, String>,
    pub query_params: BTreeMap<String, String>,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names = self.headers.keys().collect::<Vec<_>>();
        let query_param_names = self.query_params.keys().collect::<Vec<_>>();
        f.debug_struct("ProviderConfig")
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("header_names", &header_names)
            .field("query_param_names", &query_param_names)
            .finish()
    }
}

impl ProviderConfig {
    pub fn openai_chat(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::new(
            ProviderProtocol::OpenAiChatCompletions,
            base_url,
            api_key,
            model,
        )
    }

    pub fn openai_responses(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::new(ProviderProtocol::OpenAiResponses, base_url, api_key, model)
    }

    pub fn anthropic(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::new(
            ProviderProtocol::AnthropicMessages,
            base_url,
            api_key,
            model,
        )
    }

    fn new(
        protocol: ProviderProtocol,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            headers: BTreeMap::new(),
            query_params: BTreeMap::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    pub fn query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query_params.insert(name.into(), value.into());
        self
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.api_key.trim().is_empty() {
            return Err(Error::invalid_config("provider API key must not be empty"));
        }
        if self.model.trim().is_empty() {
            return Err(Error::invalid_config("provider model must not be empty"));
        }
        validate_base_url(&self.base_url, "provider")?;

        let mut normalized_headers = HashSet::new();
        for (name, value) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::invalid_config("provider header name is invalid"))?;
            if name == http::header::AUTHORIZATION || name == HeaderName::from_static("x-api-key") {
                return Err(Error::invalid_config(
                    "authentication headers are derived from protocol and api_key",
                ));
            }
            HeaderValue::from_str(value)
                .map_err(|_| Error::invalid_config("provider header value is invalid"))?;
            if !normalized_headers.insert(name) {
                return Err(Error::invalid_config(
                    "provider header names must be unique ignoring case",
                ));
            }
        }
        if self.query_params.keys().any(|name| name.trim().is_empty()) {
            return Err(Error::invalid_config(
                "provider query parameter names must not be empty",
            ));
        }
        Ok(())
    }
}

/// Explicit endpoint and credential for Grok Build's native
/// Imagine-compatible image and video tools.
///
/// Credentials and custom header values are omitted from `Debug` output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MediaProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for MediaProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaProviderConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl MediaProviderConfig {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            headers: BTreeMap::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    fn validate(&self) -> Result<(), Error> {
        if self.api_key.trim().is_empty() {
            return Err(Error::invalid_config(
                "media provider API key must not be empty",
            ));
        }
        validate_base_url(&self.base_url, "media provider")?;
        validate_headers(&self.headers, "media provider")
    }
}

/// Enables and routes Grok Build's native media tools.
///
/// The optional model overrides apply to image generation and image editing.
/// The pinned upstream uses its built-in video model for both video tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaConfig {
    pub provider: MediaProviderConfig,
    pub image_generation: bool,
    pub image_edit: bool,
    pub video_generation: bool,
    pub image_generation_model: Option<String>,
    pub image_edit_model: Option<String>,
}

impl MediaConfig {
    pub fn new(provider: MediaProviderConfig) -> Self {
        Self {
            provider,
            image_generation: true,
            image_edit: true,
            video_generation: true,
            image_generation_model: None,
            image_edit_model: None,
        }
    }

    pub fn image_generation(mut self, enabled: bool) -> Self {
        self.image_generation = enabled;
        self
    }

    pub fn image_edit(mut self, enabled: bool) -> Self {
        self.image_edit = enabled;
        self
    }

    pub fn video_generation(mut self, enabled: bool) -> Self {
        self.video_generation = enabled;
        self
    }

    pub fn image_generation_model(mut self, model: impl Into<String>) -> Self {
        self.image_generation_model = Some(model.into());
        self
    }

    pub fn image_edit_model(mut self, model: impl Into<String>) -> Self {
        self.image_edit_model = Some(model.into());
        self
    }

    fn validate(&self) -> Result<(), Error> {
        self.provider.validate()?;
        for (operation, model) in [
            ("image generation", &self.image_generation_model),
            ("image edit", &self.image_edit_model),
        ] {
            if model.as_ref().is_some_and(|model| model.trim().is_empty()) {
                return Err(Error::invalid_config(format!(
                    "{operation} model must not be empty"
                )));
            }
        }
        Ok(())
    }
}

fn validate_base_url(base_url: &str, owner: &str) -> Result<(), Error> {
    let parsed = url::Url::parse(base_url)
        .map_err(|_| Error::invalid_config(format!("{owner} base URL must be an absolute URL")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(Error::invalid_config(format!(
            "{owner} base URL must be http(s) without userinfo, a query, or a fragment"
        )));
    }
    Ok(())
}

fn validate_headers(headers: &BTreeMap<String, String>, owner: &str) -> Result<(), Error> {
    let mut normalized_headers = HashSet::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::invalid_config(format!("{owner} header name is invalid")))?;
        if name == http::header::AUTHORIZATION || name == HeaderName::from_static("x-api-key") {
            return Err(Error::invalid_config(format!(
                "{owner} authentication headers are derived from api_key"
            )));
        }
        HeaderValue::from_str(value)
            .map_err(|_| Error::invalid_config(format!("{owner} header value is invalid")))?;
        if !normalized_headers.insert(name) {
            return Err(Error::invalid_config(format!(
                "{owner} header names must be unique ignoring case"
            )));
        }
    }
    Ok(())
}

/// A public model ID mapped to an explicit provider route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelConfig {
    pub id: String,
    pub provider: ProviderConfig,
    pub context_window: NonZeroU64,
}

impl ModelConfig {
    pub fn new(id: impl Into<String>, provider: ProviderConfig) -> Self {
        Self {
            id: id.into(),
            provider,
            context_window: NonZeroU64::new(200_000).expect("constant is non-zero"),
        }
    }

    pub fn context_window(mut self, tokens: NonZeroU64) -> Self {
        self.context_window = tokens;
        self
    }
}

/// Policy applied when Grok Build requests permission to run a tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Reject tool execution. This fail-closed policy is the default.
    #[default]
    DenyAll,
    /// Select the least persistent allow option offered by Grok Build.
    AllowAll,
    /// Ask the configured [`ClientHandler`] to select an offered option.
    Delegate,
}

/// Configuration for one embedded Grok Build agent.
#[derive(Clone)]
pub struct AgentConfig {
    pub models: Vec<ModelConfig>,
    pub default_model: Option<String>,
    pub web_search_model: Option<String>,
    pub session_summary_model: Option<String>,
    pub image_description_model: Option<String>,
    pub prompt_suggestion_model: Option<String>,
    pub permission_policy: PermissionPolicy,
    pub media: Option<MediaConfig>,
    pub client_handler: Option<Arc<dyn ClientHandler>>,
}

impl fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("models", &self.models)
            .field("default_model", &self.default_model)
            .field("web_search_model", &self.web_search_model)
            .field("session_summary_model", &self.session_summary_model)
            .field("image_description_model", &self.image_description_model)
            .field("prompt_suggestion_model", &self.prompt_suggestion_model)
            .field("permission_policy", &self.permission_policy)
            .field("media", &self.media)
            .field("has_client_handler", &self.client_handler.is_some())
            .finish()
    }
}

impl AgentConfig {
    pub fn new(model: ModelConfig) -> Self {
        let default_model = Some(model.id.clone());
        Self {
            models: vec![model],
            default_model,
            web_search_model: None,
            session_summary_model: None,
            image_description_model: None,
            prompt_suggestion_model: None,
            permission_policy: PermissionPolicy::DenyAll,
            media: None,
            client_handler: None,
        }
    }

    pub fn model(mut self, model: ModelConfig) -> Self {
        self.models.push(model);
        self
    }

    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Route the native `web_search` tool through a configured Responses model.
    pub fn web_search_model(mut self, model: impl Into<String>) -> Self {
        self.web_search_model = Some(model.into());
        self
    }

    /// Route automatic titles, turn summaries, and compaction summaries.
    pub fn session_summary_model(mut self, model: impl Into<String>) -> Self {
        self.session_summary_model = Some(model.into());
        self
    }

    /// Route image attachment transcription/understanding.
    pub fn image_description_model(mut self, model: impl Into<String>) -> Self {
        self.image_description_model = Some(model.into());
        self
    }

    /// Route next-prompt suggestions requested through `x.ai/suggestPrompt`.
    pub fn prompt_suggestion_model(mut self, model: impl Into<String>) -> Self {
        self.prompt_suggestion_model = Some(model.into());
        self
    }

    pub fn permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = policy;
        self
    }

    pub fn media(mut self, media: MediaConfig) -> Self {
        self.media = Some(media);
        self
    }

    pub fn client_handler(mut self, handler: Arc<dyn ClientHandler>) -> Self {
        self.client_handler = Some(handler);
        self
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.models.is_empty() {
            return Err(Error::invalid_config("at least one model is required"));
        }
        let mut ids = HashSet::new();
        for model in &self.models {
            if model.id.trim().is_empty() {
                return Err(Error::invalid_config("model ID must not be empty"));
            }
            if !ids.insert(model.id.as_str()) {
                return Err(Error::invalid_config(format!(
                    "duplicate model ID: {}",
                    model.id
                )));
            }
            model.provider.validate()?;
        }
        if let Some(default) = &self.default_model
            && !ids.contains(default.as_str())
        {
            return Err(Error::invalid_config(format!(
                "default model is not configured: {default}"
            )));
        }
        for (capability, model) in [
            ("web search", &self.web_search_model),
            ("session summary", &self.session_summary_model),
            ("image description", &self.image_description_model),
            ("prompt suggestion", &self.prompt_suggestion_model),
        ] {
            if let Some(model) = model
                && !ids.contains(model.as_str())
            {
                return Err(Error::invalid_config(format!(
                    "{capability} model is not configured: {model}"
                )));
            }
        }
        if let Some(search_model) = &self.web_search_model {
            let protocol = self
                .models
                .iter()
                .find(|model| &model.id == search_model)
                .map(|model| model.provider.protocol);
            if protocol != Some(ProviderProtocol::OpenAiResponses) {
                return Err(Error::invalid_config(
                    "web search requires an OpenAI Responses provider",
                ));
            }
        }
        if self.permission_policy == PermissionPolicy::Delegate && self.client_handler.is_none() {
            return Err(Error::invalid_config(
                "delegated permissions require a client handler",
            ));
        }
        if let Some(media) = &self.media {
            media.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_debug_redacts_api_key() {
        let provider =
            ProviderConfig::openai_responses("https://example.com/v1", "super-secret", "model")
                .header("x-secret", "header-secret")
                .query_param("token", "query-secret");
        let debug = format!("{provider:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(debug.contains("x-secret"));
        assert!(debug.contains("token"));
        assert!(!debug.contains("super-secret"));
        assert!(!debug.contains("header-secret"));
        assert!(!debug.contains("query-secret"));
    }

    #[test]
    fn authentication_headers_are_rejected() {
        let provider = ProviderConfig::anthropic("https://example.com/v1", "key", "model")
            .header("X-Api-Key", "other");
        assert!(provider.validate().is_err());
    }

    #[test]
    fn default_model_must_exist() {
        let config = AgentConfig::new(ModelConfig::new(
            "configured",
            ProviderConfig::openai_chat("https://example.com/v1", "key", "model"),
        ))
        .default_model("missing");
        assert!(config.validate().is_err());
    }

    #[test]
    fn web_search_requires_a_responses_model() {
        let config = AgentConfig::new(ModelConfig::new(
            "chat",
            ProviderConfig::openai_chat("https://example.com/v1", "key", "model"),
        ))
        .web_search_model("chat");
        assert!(config.validate().is_err());
    }

    #[test]
    fn delegated_permissions_require_a_client_handler() {
        let config = AgentConfig::new(ModelConfig::new(
            "configured",
            ProviderConfig::openai_chat("https://example.com/v1", "key", "model"),
        ))
        .permission_policy(PermissionPolicy::Delegate);
        assert!(config.validate().is_err());
    }

    #[test]
    fn media_model_overrides_must_not_be_empty() {
        let config = AgentConfig::new(ModelConfig::new(
            "configured",
            ProviderConfig::openai_chat("https://example.com/v1", "key", "model"),
        ))
        .media(
            MediaConfig::new(MediaProviderConfig::new(
                "https://media.example.com/v1",
                "media-key",
            ))
            .image_generation_model("  "),
        );
        assert!(config.validate().is_err());
    }
}
