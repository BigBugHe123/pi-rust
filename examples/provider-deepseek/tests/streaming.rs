//! Build the real cdylib first, then test the loaded extension rather than a
//! statically constructed provider. See ../README.md for offline and live runs.
use rpi_ai::{
    types::{Content, Message, StopReason, UserMessage},
    AssistantMessageEvent, Context, Model, Provider, SimpleStreamOptions,
};
use rpi_example_provider_deepseek::{MODEL_ID, PROVIDER_ID};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const FIRST_TEXT: &str =
    "A DeepSeek extension chunk arrives before the response ends. 流式输出已到达。";
const LAST_TEXT: &str = " Second chunk.";
const REASONING: &str =
    "Checking the streaming extension path before delivering a synthetic reply. ";

fn load_extension() -> (rpi_extensions::ExtensionSession, Arc<dyn Provider>, Model) {
    let profile = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let filename = format!(
        "{}rpi_example_provider_deepseek{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let library = [
        profile.join(&filename),
        profile.join("deps").join(&filename),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .expect("build the extension first: cargo build -p rpi-example-provider-deepseek");
    let session = rpi_extensions::load_session_mixed(
        &[],
        &[PathBuf::from(library)],
        Arc::new(rpi_extensions::NullDiagnostics),
        None,
    );
    let snapshot = session.snapshot().expect("extension loaded");
    let manifest = snapshot
        .provider_manifest(PROVIDER_ID)
        .expect("DeepSeek declaration loaded from the cdylib");
    assert_eq!(manifest.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
    assert!(manifest.requires_auth);
    assert!(
        snapshot
            .providers()
            .iter()
            .any(|provider| provider.provider_id == PROVIDER_ID && provider.stream_fn.is_some()),
        "the native streaming callback must be registered"
    );
    let provider = rpi_extensions::PluggableProvider::from_session(
        &session,
        tokio::runtime::Handle::current(),
    )
    .into_iter()
    .find(|provider| provider.id() == PROVIDER_ID)
    .unwrap();
    let model = provider
        .models()
        .iter()
        .find(|model| model.id == MODEL_ID)
        .cloned()
        .unwrap();
    (session, provider, model)
}

fn context() -> Context {
    Context::new(vec![Message::User(UserMessage::new(
        "Reply briefly for a streaming smoke test.",
        0,
    ))])
}

async fn read_request(socket: &mut TcpStream) {
    let mut request = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "client closed before request body");
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
            if request.len() >= end + 4 + length {
                break end;
            }
        }
    };
    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
    assert!(headers.starts_with("post /v1/chat/completions "));
    assert!(headers.contains("authorization: bearer fixture-deepseek-key"));
    let body: serde_json::Value = serde_json::from_slice(&request[end + 4..]).unwrap();
    assert_eq!(body["model"], MODEL_ID);
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 128);
    assert_eq!(body["messages"][0]["role"], "user");
}

async fn http_chunk(socket: &mut TcpStream, bytes: &[u8]) {
    socket
        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(bytes).await.unwrap();
    socket.write_all(b"\r\n").await.unwrap();
}

fn sse(delta: serde_json::Value, finish: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({"id":"deepseek-fixture","object":"chat.completion.chunk","model":MODEL_ID,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}

async fn stub(reject: bool) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (release, gate) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        if reject {
            let body =
                r#"{"error":{"message":"fixture rejected key","type":"authentication_error"}}"#;
            socket.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            return;
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
        http_chunk(
            &mut socket,
            sse(
                serde_json::json!({"role":"assistant","reasoning_content":REASONING}),
                serde_json::Value::Null,
            )
            .as_bytes(),
        )
        .await;
        let text = sse(
            serde_json::json!({"content":FIRST_TEXT}),
            serde_json::Value::Null,
        );
        // Split an SSE event inside a multibyte UTF-8 character as HTTP chunks.
        let split = text.find('流').unwrap() + 1;
        http_chunk(&mut socket, &text.as_bytes()[..split]).await;
        http_chunk(&mut socket, &text.as_bytes()[split..]).await;
        // No finish frame or EOF until the host has observed a text delta.
        // A fake stream that waits for the whole response will time out here.
        if gate.await.is_err() {
            return;
        }
        let mut last = sse(
            serde_json::json!({"content":LAST_TEXT}),
            serde_json::json!("stop"),
        );
        last.push_str(&format!("data: {}\n\n", serde_json::json!({"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}})));
        last.push_str("data: [DONE]\n\n");
        http_chunk(&mut socket, last.as_bytes()).await;
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    (base, release, server)
}

async fn next(stream: &mut rpi_ai::AssistantMessageEventStream) -> Option<AssistantMessageEvent> {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("extension must deliver an event without waiting for server EOF")
}

fn options() -> SimpleStreamOptions {
    SimpleStreamOptions {
        api_key: Some("fixture-deepseek-key".into()),
        max_tokens: Some(128),
        max_retries: Some(0),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_extension_delivers_reasoning_and_text_before_server_finishes() {
    let (_session, provider, mut model) = load_extension();
    let (base, release, server) = stub(false).await;
    model.base_url = base;
    let mut stream = provider.stream_simple(&model, &context(), &options()).await;
    let mut tags = vec![];
    let mut text = String::new();
    let mut reasoning = String::new();
    loop {
        let event = next(&mut stream).await.expect("text before terminal");
        tags.push(event.type_tag());
        match event {
            AssistantMessageEvent::ThinkingDelta { delta, .. } => reasoning.push_str(&delta),
            AssistantMessageEvent::TextDelta { delta, .. } => {
                text.push_str(&delta);
                break;
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                panic!("terminal before incremental output")
            }
            _ => {}
        }
    }
    assert_eq!(reasoning, REASONING);
    assert_eq!(text, FIRST_TEXT);
    assert!(
        !server.is_finished(),
        "delta must arrive while response is still open"
    );
    release.send(()).unwrap();
    while let Some(event) = next(&mut stream).await {
        tags.push(event.type_tag());
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
            AssistantMessageEvent::Error { error, .. } => {
                panic!("unexpected error: {:?}", error.stop_reason)
            }
            _ => {}
        }
    }
    let message = stream.result().await.unwrap();
    assert_eq!(message.provider, PROVIDER_ID);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(text, format!("{FIRST_TEXT}{LAST_TEXT}"));
    assert_eq!(Content::text_only(&message.content, ""), text);
    assert_eq!(message.usage.total_tokens, 12);
    assert_eq!(tags.first(), Some(&"start"));
    assert_eq!(tags.last(), Some(&"done"));
    assert_eq!(tags.iter().filter(|tag| **tag == "done").count(), 1);
    assert_eq!(tags.iter().filter(|tag| **tag == "text_delta").count(), 2);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_extension_cancels_an_open_response_and_retains_partial_text() {
    let (_session, provider, mut model) = load_extension();
    let (base, release, server) = stub(false).await;
    model.base_url = base;
    let options = options();
    let mut stream = provider.stream_simple(&model, &context(), &options).await;
    loop {
        match next(&mut stream)
            .await
            .expect("partial text before cancellation")
        {
            AssistantMessageEvent::TextDelta { .. } => break,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                panic!("unexpected terminal before cancel")
            }
            _ => {}
        }
    }
    options.signal.cancel();
    let mut terminal = vec![];
    while let Some(event) = next(&mut stream).await {
        if event.is_terminal() {
            terminal.push(event.type_tag());
        }
    }
    let message = stream.result().await.unwrap();
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(Content::text_only(&message.content, ""), FIRST_TEXT);
    assert_eq!(terminal, ["error"]);
    drop(release); // The server never sends a successful finish frame.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_extension_reports_auth_errors_without_a_success_terminal() {
    let (_session, provider, mut model) = load_extension();
    let (base, _release, server) = stub(true).await;
    model.base_url = base;
    let mut stream = provider.stream_simple(&model, &context(), &options()).await;
    let mut terminal = vec![];
    while let Some(event) = next(&mut stream).await {
        if event.is_terminal() {
            terminal.push(event.type_tag());
        }
    }
    let message = stream.result().await.unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message.error_message.as_deref().unwrap().contains("401"));
    assert_eq!(terminal, ["error"]);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live API smoke test; requires DEEPSEEK_API_KEY and consumes API quota"]
async fn live_deepseek_extension_stream() {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .expect("set DEEPSEEK_API_KEY before opting into the live test");
    let (_session, provider, model) = load_extension();
    let options = SimpleStreamOptions {
        api_key: Some(key),
        max_tokens: Some(1_024),
        max_retries: Some(0),
        timeout: Some(Duration::from_secs(45)),
        ..Default::default()
    };
    let mut stream = provider.stream_simple(&model, &context(), &options).await;
    let mut deltas = 0;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .expect("live stream timed out")
    {
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                deltas += 1;
                eprint!("{delta}");
            }
            AssistantMessageEvent::Error { error, .. } => {
                panic!("DeepSeek request failed: {:?}", error.stop_reason)
            }
            _ => {}
        }
    }
    let message = stream.result().await.unwrap();
    assert!(matches!(
        message.stop_reason,
        StopReason::Stop | StopReason::Length
    ));
    assert!(deltas > 0 && !Content::text_only(&message.content, "").is_empty());
    eprintln!("\nprovider={} text_delta_events={deltas}", message.provider);
}
