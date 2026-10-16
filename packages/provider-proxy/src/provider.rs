//! Proxy provider — routes LLM calls through a proxy server.
//!
//! Mirrors `packages/agent/src/proxy.ts`. Allows routing LLM API calls through
//! an intermediate server that handles authentication and request forwarding.
//!
//! # Use Cases
//!
//! - **Centralized authentication**: Proxy manages API keys, clients don't need credentials
//! - **Rate limiting**: Proxy enforces usage limits across multiple clients
//! - **Audit logging**: Proxy logs all requests for compliance
//! - **Cost control**: Proxy tracks and limits spending
//!
//! # Architecture
//!
//! ```text
//! Client (rpi) → Proxy Server → LLM Provider (Anthropic/OpenAI)
//!      ↑              ↑
//!   No API key    Manages auth
//! ```
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use rpi_provider_proxy::{ProxyConfig, ProxyProvider};
//!
//! let config = ProxyConfig {
//!     endpoint: "https://proxy.example.com/v1".to_string(),
//!     api_key: Some("proxy-token".to_string()),
//!     timeout: Some(Duration::from_secs(60)),
//!     // `headers`/`api_key` are optional; the proxy holds the real provider key.
//!     headers: None,
//! };
//!
//! let _provider = ProxyProvider::new(config);
//! // Call `stream_simple(&model, &ctx, &opts)` from an async context to use it.
//! ```

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rpi_ai::event_stream::AssistantMessageEventStream;
use rpi_ai::model::Model;
use rpi_ai::provider::{Provider, SimpleStreamOptions};
use rpi_ai::types::Context;

/// Configuration for a proxy provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// The proxy server endpoint (e.g., "https://proxy.example.com/v1").
    pub endpoint: String,
    /// Optional API key for the proxy (not the underlying LLM provider).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Request timeout. Defaults to 600 seconds if not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Additional headers to send with each request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

impl ProxyConfig {
    /// Create a new proxy configuration.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: None,
            timeout: None,
            headers: None,
        }
    }

    /// Set the API key.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Set the timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Add a header.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let headers = self.headers.get_or_insert_with(Default::default);
        headers.insert(key.into(), value.into());
        self
    }
}

/// A provider that routes requests through a proxy server.
///
/// The proxy server handles authentication with the underlying LLM provider
/// and forwards requests. This allows clients to use the proxy without
/// needing direct API credentials.
pub struct ProxyProvider {
    config: ProxyConfig,
}

impl ProxyProvider {
    /// Create a new proxy provider.
    pub fn new(config: ProxyConfig) -> Self {
        Self { config }
    }

    /// Get the proxy endpoint.
    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }
}

#[async_trait]
impl Provider for ProxyProvider {
    fn id(&self) -> &str {
        "proxy"
    }

    fn models(&self) -> &[Model] {
        // Proxy providers don't have a fixed model list; they forward to any model
        // the proxy server supports. Return empty slice; the actual model list
        // would come from the proxy server's /models endpoint.
        &[]
    }

    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let mut model = model.clone();
        if model.base_url.is_empty() {
            model.base_url = self.config.endpoint.clone();
        }
        let mut options = options.clone();
        if options.api_key.is_none() {
            options.api_key = self.config.api_key.clone();
        }
        if let Some(headers) = &self.config.headers {
            let options_headers = options.headers.get_or_insert_with(Default::default);
            for (key, value) in headers {
                options_headers
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
        rpi_provider_runtime::completions(&model, &options, true)
            .stream_simple(&model, context, &options)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_builder() {
        let config = ProxyConfig::new("https://proxy.example.com")
            .with_api_key("test-key")
            .with_timeout(Duration::from_secs(30))
            .with_header("X-Custom", "value");

        assert_eq!(config.endpoint, "https://proxy.example.com");
        assert_eq!(config.api_key, Some("test-key".to_string()));
        assert_eq!(config.timeout, Some(Duration::from_secs(30)));
        assert_eq!(
            config.headers.as_ref().unwrap().get("X-Custom"),
            Some(&"value".to_string())
        );
    }

    #[test]
    fn test_proxy_provider_creation() {
        let config = ProxyConfig::new("https://proxy.example.com");
        let provider = ProxyProvider::new(config);
        assert_eq!(provider.endpoint(), "https://proxy.example.com");
        assert_eq!(provider.id(), "proxy");
    }
}
