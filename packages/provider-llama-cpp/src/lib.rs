//! Local inference and model management, packaged separately from rpi-ai.
pub mod command;
pub mod provider;
pub use provider::{LlamaCppConfig, LlamaCppModelManager, LlamaCppProvider};
use rpi_ai::{Api, Model, Provider, SimpleStreamOptions};
use std::sync::Arc;

fn model() -> Model {
    let mut model = Model::new(
        "local",
        "Local llama.cpp model",
        Api::OpenaiCompletions,
        "llama-cpp",
        "http://localhost:8080/v1",
    );
    model.context_window = 32_768;
    model.max_tokens = 4_096;
    model
}

fn create(model: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
    Arc::new(LlamaCppProvider::new(LlamaCppConfig {
        server_url: model
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string(),
        timeout: options.request_timeout(),
        api_key: options.api_key.clone(),
    }))
}

rpi_provider_runtime::export_provider!(
    "llama-cpp",
    "http://localhost:8080/v1",
    "openai-completions",
    serde_json::json!({"id":"llama-cpp", "models":[model()], "apiKeyEnv":"LLAMA_CPP_API_KEY", "requiresAuth":false}),
    create
);
