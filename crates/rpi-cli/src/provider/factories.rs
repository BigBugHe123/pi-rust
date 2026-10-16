//! Built-in protocols and native extensions implement the same factory contract.
//! Authentication and model binding are applied once, outside every factory.
use super::binding::{ExecutionBinding, RequestAuth};
use super::*;
use rpi_extensions::{ExtensionSession, ProviderManifest};

trait ProviderFactory: Send + Sync {
    fn create(
        &self,
        provider_id: &str,
        models: Vec<Model>,
        settings: &settings::Settings,
    ) -> Arc<dyn Provider>;
}

struct ProtocolFactory(rpi_ai::Api);
impl ProviderFactory for ProtocolFactory {
    fn create(
        &self,
        provider_id: &str,
        models: Vec<Model>,
        settings: &settings::Settings,
    ) -> Arc<dyn Provider> {
        let fallback = if self.0 == rpi_ai::Api::AnthropicMessages {
            "https://api.anthropic.com"
        } else {
            "https://api.openai.com/v1"
        };
        let http = provider_http_client(&models, fallback, settings);
        match self.0 {
            rpi_ai::Api::AnthropicMessages => Arc::new(
                AnthropicProvider::with_models_without_env_api_key(None, http, models),
            ),
            rpi_ai::Api::OpenaiCompletions => {
                Arc::new(OpenAiCompletionsProvider::with_models_without_env_api_key(
                    provider_id,
                    None,
                    http,
                    models,
                ))
            }
            rpi_ai::Api::OpenaiResponses => {
                Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
                    provider_id,
                    None,
                    http,
                    models,
                ))
            }
            _ => unreachable!("only supported wire protocols are registered"),
        }
    }
}

struct ExtensionFactory(Arc<dyn Provider>);
impl ProviderFactory for ExtensionFactory {
    fn create(&self, _: &str, _: Vec<Model>, _: &settings::Settings) -> Arc<dyn Provider> {
        self.0.clone()
    }
}

pub(super) struct RuntimeFactories {
    protocols: BTreeMap<String, Arc<dyn ProviderFactory>>,
    providers: BTreeMap<String, Arc<dyn ProviderFactory>>,
    pub manifests: Vec<ProviderManifest>,
}

impl Default for RuntimeFactories {
    fn default() -> Self {
        let protocols = [
            rpi_ai::Api::AnthropicMessages,
            rpi_ai::Api::OpenaiCompletions,
            rpi_ai::Api::OpenaiResponses,
        ]
        .into_iter()
        .map(|api| {
            (
                api.as_str().to_string(),
                Arc::new(ProtocolFactory(api)) as Arc<dyn ProviderFactory>,
            )
        })
        .collect();
        Self {
            protocols,
            providers: BTreeMap::new(),
            manifests: Vec::new(),
        }
    }
}

impl RuntimeFactories {
    pub fn from_session(session: &ExtensionSession) -> Self {
        let mut factories = Self::default();
        if session.snapshot().is_some() {
            factories.providers = rpi_extensions::PluggableProvider::from_session(
                session,
                tokio::runtime::Handle::current(),
            )
            .into_iter()
            .map(|provider| {
                (
                    provider.id().to_string(),
                    Arc::new(ExtensionFactory(provider)) as Arc<dyn ProviderFactory>,
                )
            })
            .collect();
            factories.manifests = manifests_from_session(session);
        }
        factories
    }

    pub fn configure(
        &self,
        provider_id: &str,
        api: &rpi_ai::Api,
        models: Vec<Model>,
        auth: RequestAuth,
        settings: &settings::Settings,
    ) -> Option<(Arc<dyn Provider>, Arc<ExecutionBinding>)> {
        let factory = self
            .providers
            .get(provider_id)
            .or_else(|| self.protocols.get(api.as_str()))?;
        let binding = Arc::new(ExecutionBinding::new(provider_id, models, auth));
        let inner = factory.create(provider_id, binding.models().to_vec(), settings);
        let provider = Arc::new(ConfiguredProvider {
            id: provider_id.into(),
            inner,
            binding: binding.clone(),
        });
        Some((provider, binding))
    }
}

struct ConfiguredProvider {
    id: String,
    inner: Arc<dyn Provider>,
    binding: Arc<ExecutionBinding>,
}

#[async_trait::async_trait]
impl Provider for ConfiguredProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        self.binding.models()
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let Some((model, options)) = self.binding.prepare(model, options) else {
            let (mut producer, stream) =
                rpi_ai::event_stream::create_assistant_message_event_stream();
            producer.push(rpi_ai::AssistantMessageEvent::Error {
                reason: rpi_ai::types::ErrorReason::Error,
                error: rpi_ai::types::AssistantMessage::terminal(
                    model.api.clone(),
                    self.id.clone(),
                    model.id.clone(),
                    rpi_ai::types::StopReason::Error,
                    "model is not registered in this provider execution binding",
                    0,
                ),
            });
            return stream;
        };
        self.inner.stream_simple(&model, context, &options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::{
        types::{AssistantMessage, DoneReason, ErrorReason, StopReason},
        Api, AssistantMessageEvent,
    };
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(Model, SimpleStreamOptions)>>>;
    struct RecordingFactory(Calls);
    struct RecordingProvider(Calls);
    impl ProviderFactory for RecordingFactory {
        fn create(&self, _: &str, models: Vec<Model>, _: &settings::Settings) -> Arc<dyn Provider> {
            assert!(
                models.iter().all(|model| model.headers.is_none()),
                "factories receive public descriptors"
            );
            Arc::new(RecordingProvider(self.0.clone()))
        }
    }
    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        fn id(&self) -> &str {
            "recording"
        }
        fn models(&self) -> &[Model] {
            &[]
        }
        async fn stream_simple(
            &self,
            model: &Model,
            _: &Context,
            options: &SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            self.0
                .lock()
                .unwrap()
                .push((model.clone(), options.clone()));
            let (mut producer, stream) =
                rpi_ai::event_stream::create_assistant_message_event_stream();
            producer.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: AssistantMessage::empty(
                    model.api.clone(),
                    model.provider.clone(),
                    model.id.clone(),
                    0,
                ),
            });
            stream
        }
    }

    #[tokio::test]
    async fn protocol_and_extension_factories_use_the_same_private_binding() {
        for extension in [false, true] {
            let calls = Calls::default();
            let mut factories = RuntimeFactories::default();
            let factory = Arc::new(RecordingFactory(calls.clone())) as Arc<dyn ProviderFactory>;
            if extension {
                factories.providers.insert("account".into(), factory);
            } else {
                factories
                    .protocols
                    .insert("openai-completions".into(), factory);
            }
            let mut raw = Model::new(
                "model",
                "Model",
                Api::OpenaiCompletions,
                "account",
                "https://configured.example/v1",
            );
            raw.headers = Some(BTreeMap::from([(
                "x-api-key".into(),
                "private-header".into(),
            )]));
            let (provider, binding) = factories
                .configure(
                    "account",
                    &raw.api.clone(),
                    vec![raw],
                    RequestAuth::ApiKey("private-credential".into()),
                    &settings::Settings::default(),
                )
                .unwrap();
            let serialized = serde_json::to_string(provider.models()).unwrap();
            assert!(
                !serialized.contains("private-header")
                    && !serialized.contains("private-credential")
            );
            assert!(!format!("{binding:?}").contains("private-credential"));
            let mut requested = provider.models()[0].clone();
            requested.base_url = "https://untrusted.example/v1".into();
            requested.headers = Some(BTreeMap::from([(
                "authorization".into(),
                "injected".into(),
            )]));
            provider
                .stream_simple(
                    &requested,
                    &Context::new(vec![]),
                    &SimpleStreamOptions::default(),
                )
                .await
                .result()
                .await
                .unwrap();
            let recorded = calls.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].0.base_url, "https://configured.example/v1");
            assert_eq!(
                recorded[0].0.headers.as_ref().unwrap()["x-api-key"],
                "private-header"
            );
            assert_eq!(recorded[0].1.api_key.as_deref(), Some("private-credential"));
            assert!(!recorded[0]
                .0
                .headers
                .as_ref()
                .unwrap()
                .contains_key("authorization"));
            drop(recorded);
            requested.provider = "another-account".into();
            let mut rejected = provider
                .stream_simple(
                    &requested,
                    &Context::new(vec![]),
                    &SimpleStreamOptions::default(),
                )
                .await;
            assert!(matches!(
                rejected.next().await,
                Some(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    ..
                })
            ));
            assert_eq!(
                rejected.result().await.unwrap().stop_reason,
                StopReason::Error
            );
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn request_overrides_and_model_headers_remain_scoped_to_their_execution() {
        let calls = Calls::default();
        let mut factories = RuntimeFactories::default();
        factories
            .providers
            .insert("gateway".into(), Arc::new(RecordingFactory(calls.clone())));
        let mut models = vec![];
        for (id, token) in [("first", "first-token"), ("second", "second-token")] {
            let mut model = Model::new(
                id,
                id,
                Api::AnthropicMessages,
                "gateway",
                "https://gateway.example",
            );
            model.headers = Some(BTreeMap::from([("x-api-key".into(), token.into())]));
            models.push(model);
        }
        let (provider, _) = factories
            .configure(
                "gateway",
                &Api::AnthropicMessages,
                models,
                RequestAuth::Headers(BTreeMap::from([(
                    "authorization".into(),
                    "Bearer default".into(),
                )])),
                &settings::Settings::default(),
            )
            .unwrap();
        let options = SimpleStreamOptions {
            headers: Some(BTreeMap::from([(
                "Authorization".into(),
                "Bearer per-request".into(),
            )])),
            ..Default::default()
        };
        for model in provider.models() {
            provider
                .stream_simple(model, &Context::new(vec![]), &options)
                .await
                .result()
                .await
                .unwrap();
        }
        let recorded = calls.lock().unwrap();
        assert_eq!(
            recorded[0].0.headers.as_ref().unwrap()["x-api-key"],
            "first-token"
        );
        assert_eq!(
            recorded[1].0.headers.as_ref().unwrap()["x-api-key"],
            "second-token"
        );
        assert_eq!(recorded[0].1.headers.as_ref().unwrap().len(), 1);
        assert_eq!(
            recorded[0].1.headers.as_ref().unwrap()["Authorization"],
            "Bearer per-request"
        );
    }
}
