//! Optional OpenAI-compatible proxy provider.
pub mod provider;
pub use provider::{ProxyConfig, ProxyProvider};
use rpi_ai::{Api, Model, Provider, SimpleStreamOptions};
use std::sync::Arc;

fn model() -> Model {
    let mut model = Model::new(
        "default",
        "Proxy default model",
        Api::OpenaiCompletions,
        "proxy",
        "http://localhost:8080/v1",
    );
    model.context_window = 32_768;
    model.max_tokens = 4_096;
    model
}

fn create(model: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
    Arc::new(ProxyProvider::new(ProxyConfig {
        endpoint: model.base_url.clone(),
        api_key: options.api_key.clone(),
        timeout: options.timeout,
        headers: options.headers.clone(),
    }))
}

rpi_provider_runtime::export_provider!(
    "proxy",
    "http://localhost:8080/v1",
    "openai-completions",
    serde_json::json!({"id":"proxy", "models":[model()], "apiKeyEnv":"PROXY_API_KEY", "requiresAuth":false}),
    create
);
