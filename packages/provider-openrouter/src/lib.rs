//! OpenRouter integration, installed independently from the rpi CLI.
use rpi_ai::{Api, AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};
use std::{collections::BTreeMap, sync::Arc};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouterProvider {
    api_key: Option<String>,
    models: Vec<Model>,
}

impl OpenRouterProvider {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            api_key,
            models: vec![default_model()],
        }
    }
}

#[async_trait::async_trait]
impl Provider for OpenRouterProvider {
    fn id(&self) -> &str {
        "openrouter"
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let mut options = options.clone();
        if options.api_key.is_none() {
            options.api_key = self.api_key.clone();
        }
        let headers = options.headers.get_or_insert_with(BTreeMap::new);
        for (name, value) in [
            ("HTTP-Referer", "https://rpi.laofu.online/"),
            ("X-Title", "rpi"),
        ] {
            if !headers.keys().any(|key| key.eq_ignore_ascii_case(name)) {
                headers.insert(name.into(), value.into());
            }
        }
        rpi_provider_runtime::completions(model, &options, false)
            .stream_simple(model, context, &options)
            .await
    }
}

fn default_model() -> Model {
    let mut model = Model::new(
        "openrouter/auto",
        "OpenRouter Auto",
        Api::OpenaiCompletions,
        "openrouter",
        BASE_URL,
    );
    model.context_window = 128_000;
    model.max_tokens = 16_384;
    model
}

fn create(model: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
    Arc::new(OpenRouterProvider {
        api_key: options.api_key.clone(),
        models: vec![model.clone()],
    })
}

rpi_provider_runtime::export_provider!(
    "openrouter",
    BASE_URL,
    "openai-completions",
    serde_json::json!({"id":"openrouter", "models":[default_model()], "apiKeyEnv":"OPENROUTER_API_KEY", "requiresAuth":true}),
    create
);
