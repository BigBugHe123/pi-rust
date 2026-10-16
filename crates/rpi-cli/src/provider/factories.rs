//! Runtime factories for provider packages. The selected provider's credentials
//! are bound here, never shared with another package or protocol implementation.
use super::*;
use rpi_extensions::{ExtensionSession, ProviderManifest};

#[derive(Default)]
pub(super) struct RuntimeFactories {
    pub providers: BTreeMap<String, Arc<dyn Provider>>,
    pub manifests: Vec<ProviderManifest>,
}

impl RuntimeFactories {
    pub fn from_session(session: &ExtensionSession) -> Self {
        let Some(_) = session.snapshot() else {
            return Self::default();
        };
        let providers = rpi_extensions::PluggableProvider::from_session(
            session,
            tokio::runtime::Handle::current(),
        )
        .into_iter()
        .map(|provider| (provider.id().to_string(), provider))
        .collect();
        let manifests = manifests_from_session(session);
        Self {
            providers,
            manifests,
        }
    }

    pub fn configure(
        &self,
        provider_id: &str,
        models: Vec<Model>,
        api_key: Option<String>,
    ) -> Option<Arc<dyn Provider>> {
        self.providers.get(provider_id).map(|provider| {
            Arc::new(ConfiguredProvider {
                inner: Arc::clone(provider),
                models,
                api_key,
            }) as Arc<dyn Provider>
        })
    }
}

struct ConfiguredProvider {
    inner: Arc<dyn Provider>,
    models: Vec<Model>,
    api_key: Option<String>,
}

#[async_trait::async_trait]
impl Provider for ConfiguredProvider {
    fn id(&self) -> &str {
        self.inner.id()
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
        self.inner.stream_simple(model, context, &options).await
    }
}
