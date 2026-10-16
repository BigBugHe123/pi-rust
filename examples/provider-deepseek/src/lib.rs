//! An independently loaded DeepSeek extension for streaming smoke tests.
//! A distinct id proves that requests go through the native extension ABI.
use rpi_ai::{Api, Model, Provider, SimpleStreamOptions};
use std::sync::Arc;

pub const PROVIDER_ID: &str = "deepseek-extension";
pub const MODEL_ID: &str = "deepseek-flash";
const BASE_URL: &str = "https://api.deepseek.com/v1";

pub fn models() -> Vec<Model> {
    let mut model = Model::new(
        MODEL_ID,
        "DeepSeek Flash (extension)",
        Api::OpenaiCompletions,
        PROVIDER_ID,
        BASE_URL,
    );
    // Conservative example limits; override with models.json for your account.
    model.context_window = 32_768;
    model.max_tokens = 4_096;
    model.reasoning = true;
    model.compat = Some(rpi_ai::model::StreamingProtocolCompat::OpenaiCompletions(
        rpi_ai::model::OpenaiCompletionsCompat {
            supports_store: Some(false),
            supports_developer_role: Some(false),
            supports_strict_mode: Some(false),
            supports_reasoning_effort: Some(true),
            max_tokens_field: Some(rpi_ai::model::MaxTokensField::MaxTokens),
            ..Default::default()
        },
    ));
    vec![model]
}

fn create(model: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
    // Reuse the maintained protocol parser for text, reasoning_content, usage,
    // HTTP errors and cancellation. Credentials are provided by the host.
    rpi_provider_runtime::completions(model, options, false)
}

rpi_provider_runtime::export_provider!(
    PROVIDER_ID,
    BASE_URL,
    "openai-completions",
    serde_json::json!({"id":PROVIDER_ID,"models":models(),"apiKeyEnv":"DEEPSEEK_API_KEY","requiresAuth":true}),
    create
);
