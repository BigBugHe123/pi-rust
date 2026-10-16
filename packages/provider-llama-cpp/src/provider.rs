//! LLaMA.cpp provider integration.
//!
//! Provides support for local LLM inference using llama.cpp server.
//! This allows users to run models locally without API keys.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use rpi_ai::error::AiError;
use rpi_ai::event_stream::AssistantMessageEventStream;
use rpi_ai::model::Model;
use rpi_ai::provider::{Provider, SimpleStreamOptions};
use rpi_ai::types::Context;

/// LLaMA.cpp server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlamaCppConfig {
    /// Server URL (default: http://localhost:8080)
    #[serde(default = "default_server_url")]
    pub server_url: String,

    /// Request timeout
    #[serde(default = "default_timeout")]
    pub timeout: Duration,

    /// API key (if required)
    pub api_key: Option<String>,
}

fn default_server_url() -> String {
    "http://localhost:8080".to_string()
}

fn default_timeout() -> Duration {
    Duration::from_secs(300)
}

impl Default for LlamaCppConfig {
    fn default() -> Self {
        Self {
            server_url: default_server_url(),
            timeout: default_timeout(),
            api_key: None,
        }
    }
}

/// LLaMA.cpp provider
pub struct LlamaCppProvider {
    config: LlamaCppConfig,
    client: Client,
}

impl LlamaCppProvider {
    /// Create a new LLaMA.cpp provider
    pub fn new(config: LlamaCppConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("Failed to build HTTP client");

        Self { config, client }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(LlamaCppConfig::default())
    }

    /// Check if the server is available
    pub async fn health_check(&self) -> Result<bool, AiError> {
        let url = format!("{}/health", self.config.server_url);

        let mut request = self.client.get(&url);

        if let Some(api_key) = &self.config.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request.send().await.map_err(|e| AiError::Http {
            status: None,
            message: format!("Request failed: {}", e),
        })?;

        Ok(response.status().is_success())
    }

    /// Get model information from the server
    pub async fn get_model_info(&self) -> Result<serde_json::Value, AiError> {
        let url = format!("{}/v1/models", self.config.server_url);

        let mut request = self.client.get(&url);

        if let Some(api_key) = &self.config.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request.send().await.map_err(|e| AiError::Http {
            status: None,
            message: format!("Request failed: {}", e),
        })?;

        if !response.status().is_success() {
            return Err(AiError::Http {
                status: Some(response.status().as_u16()),
                message: format!("Failed to get model info: {}", response.status()),
            });
        }

        let info: serde_json::Value = response.json().await.map_err(|e| AiError::Http {
            status: None,
            message: format!("Failed to parse response: {}", e),
        })?;
        Ok(info)
    }
}

#[async_trait]
impl Provider for LlamaCppProvider {
    fn id(&self) -> &str {
        "llama-cpp"
    }

    fn models(&self) -> &[Model] {
        // LLaMA.cpp models are dynamically loaded from the server
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
            model.base_url = format!("{}/v1", self.config.server_url.trim_end_matches('/'));
        }
        let mut options = options.clone();
        if options.api_key.is_none() {
            options.api_key = self.config.api_key.clone();
        }
        rpi_provider_runtime::completions(&model, &options, true)
            .stream_simple(&model, context, &options)
            .await
    }
}

/// LLaMA.cpp model manager
pub struct LlamaCppModelManager {
    provider: LlamaCppProvider,
}

impl LlamaCppModelManager {
    /// Create a new model manager
    pub fn new(config: LlamaCppConfig) -> Self {
        Self {
            provider: LlamaCppProvider::new(config),
        }
    }

    /// List available models
    pub async fn list_models(&self) -> Result<Vec<String>, AiError> {
        let info = self.provider.get_model_info().await?;

        let models = info["data"]
            .as_array()
            .ok_or_else(|| AiError::Provider {
                code: "INVALID_RESPONSE".to_string(),
                message: "Invalid model list format".to_string(),
            })?
            .iter()
            .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
            .collect();

        Ok(models)
    }

    /// Check if server is healthy
    pub async fn is_healthy(&self) -> Result<bool, AiError> {
        self.provider.health_check().await
    }

    /// Get the provider
    pub fn provider(&self) -> &LlamaCppProvider {
        &self.provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = LlamaCppConfig::default();
        assert_eq!(config.server_url, "http://localhost:8080");
        assert_eq!(config.timeout, Duration::from_secs(300));
        assert!(config.api_key.is_none());
    }

    #[test]
    fn test_provider_creation() {
        let provider = LlamaCppProvider::with_defaults();
        assert_eq!(provider.id(), "llama-cpp");
    }
}
