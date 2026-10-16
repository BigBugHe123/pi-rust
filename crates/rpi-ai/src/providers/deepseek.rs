//! DeepSeek compatibility provider backed by the maintained Chat Completions
//! implementation. Authentication, reasoning deltas, tools, retries and aborts
//! share the same protocol path as configured OpenAI-compatible providers.
use crate::{AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};

const DEEPSEEK_API_BASE: &str = "https://api.deepseek.com/v1";

#[derive(Debug, Clone)]
pub struct DeepSeekProvider {
    api_key: Option<String>,
}

impl DeepSeekProvider {
    pub fn new(api_key: Option<String>) -> Self {
        Self { api_key }
    }
}

#[async_trait::async_trait]
impl Provider for DeepSeekProvider {
    fn id(&self) -> &str {
        "deepseek"
    }
    fn models(&self) -> &[Model] {
        &[]
    }

    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let mut model = model.clone();
        model.provider = self.id().into();
        if model.base_url.is_empty() {
            model.base_url = DEEPSEEK_API_BASE.into();
        }
        if model.compat.is_none() {
            model.compat = Some(crate::model::StreamingProtocolCompat::OpenaiCompletions(
                crate::model::OpenaiCompletionsCompat {
                    supports_store: Some(false),
                    supports_developer_role: Some(false),
                    supports_strict_mode: Some(false),
                    max_tokens_field: Some(crate::model::MaxTokensField::MaxTokens),
                    ..Default::default()
                },
            ));
        }
        let http = crate::http::build_client(&model.base_url, None, None)
            .unwrap_or_else(|_| reqwest::Client::new());
        let provider =
            super::openai_completions::OpenAiCompletionsProvider::with_models_without_env_api_key(
                self.id(),
                self.api_key.clone(),
                http,
                vec![model.clone()],
            );
        provider.stream_simple(&model, context, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Content, Message, StopReason, UserMessage};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_deepseek_provider_creation() {
        assert_eq!(
            DeepSeekProvider::new(Some("test-key".into())).id(),
            "deepseek"
        );
    }

    #[tokio::test]
    async fn deepseek_wrapper_streams_before_eof_and_honors_the_model_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let text = "The shared DeepSeek protocol delivers this partial output before the server completes its response.";
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() < end + 4 + length {
                        continue;
                    }
                    assert!(headers.starts_with("post /v1/chat/completions "));
                    assert!(headers.contains("authorization: bearer request-key"));
                    let body: serde_json::Value =
                        serde_json::from_slice(&request[end + 4..]).unwrap();
                    assert_eq!(body["stream"], true);
                    assert_eq!(body["messages"][0]["content"], "hello");
                    break;
                }
            }
            let first = format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})
            );
            let last = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", first.len()+last.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(first.as_bytes()).await.unwrap();
            gate.await.unwrap();
            socket.write_all(last.as_bytes()).await.unwrap();
        });
        let provider = DeepSeekProvider::new(Some("provider-key".into()));
        let model = Model::new(
            "deepseek-flash",
            "DeepSeek",
            crate::Api::OpenaiCompletions,
            "deepseek",
            base,
        );
        let options = SimpleStreamOptions {
            api_key: Some("request-key".into()),
            max_retries: Some(0),
            ..Default::default()
        };
        let context = Context::new(vec![Message::User(UserMessage::new("hello", 0))]);
        let mut stream = provider.stream_simple(&model, &context, &options).await;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                .await
                .unwrap()
                .unwrap();
            match event {
                crate::AssistantMessageEvent::TextDelta { delta, .. } => {
                    assert_eq!(delta, text);
                    break;
                }
                crate::AssistantMessageEvent::Done { .. }
                | crate::AssistantMessageEvent::Error { .. } => {
                    panic!("terminal before partial output")
                }
                _ => {}
            }
        }
        release.send(()).unwrap();
        while stream.next().await.is_some() {}
        let message = stream.result().await.unwrap();
        assert_eq!(message.provider, "deepseek");
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(Content::text_only(&message.content, ""), text);
        server.await.unwrap();
    }
}
