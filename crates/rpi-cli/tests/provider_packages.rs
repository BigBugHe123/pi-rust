//! Build packages first: cargo build -p rpi-provider-openrouter
//! -p rpi-provider-llama-cpp -p rpi-provider-proxy
//! Then run cargo test -p rpi-cli --test provider_packages.
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn library(name: &str) -> Option<PathBuf> {
    let profile = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .to_path_buf();
    let file = if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    };
    [profile.join(&file), profile.join("deps").join(file)]
        .into_iter()
        .find(|path| path.is_file())
}

fn command(temp: &tempfile::TempDir, library: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rpi"));
    cmd.current_dir(temp.path())
        .stdin(Stdio::null())
        .env("RPI_CODING_AGENT_DIR", temp.path().join("agent"))
        .env("RPI_OFFLINE", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("LLAMA_CPP_API_KEY")
        .env_remove("PROXY_API_KEY")
        .arg("--extension")
        .arg(library);
    cmd
}

#[test]
fn package_catalog_is_available_without_credentials_and_respects_no_extensions() {
    let Some(library) = library("rpi_provider_openrouter") else {
        eprintln!("SKIP: build rpi-provider-openrouter first");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let output = command(&temp, &library)
        .args(["--list-models", "openrouter"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("openrouter/auto"));
    let output = command(&temp, &library)
        .args(["--no-extensions", "--list-models", "openrouter"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("openrouter/auto"));
}

#[test]
fn openrouter_package_does_not_inherit_openai_credentials() {
    let Some(library) = library("rpi_provider_openrouter") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let output = command(&temp, &library)
        .env("OPENAI_API_KEY", "unrelated-openai-key")
        .args([
            "--provider",
            "openrouter",
            "--model",
            "openrouter/auto",
            "--print",
            "--no-session",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No API key"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("unrelated-openai-key"));
}

#[tokio::test]
async fn packaged_providers_stream_through_the_real_dll_and_cli() {
    for (name, provider, model, auth) in [
        (
            "rpi_provider_openrouter",
            "openrouter",
            "openrouter/auto",
            true,
        ),
        ("rpi_provider_llama_cpp", "llama-cpp", "local", false),
        ("rpi_provider_proxy", "proxy", "default", false),
    ] {
        let Some(library) = library(name) else {
            eprintln!("SKIP: build {name} first");
            continue;
        };
        let temp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0u8; 8192];
                let n = socket.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0, "request closed before body");
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]);
                    let length: usize = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .map(str::to_string)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.starts_with("POST /v1/chat/completions "));
            if auth {
                assert!(text
                    .to_ascii_lowercase()
                    .contains("authorization: bearer package-key"));
                assert!(
                    text.to_ascii_lowercase().contains("x-title: rpi"),
                    "the package factory must handle this request"
                );
            } else {
                assert!(!text.to_ascii_lowercase().contains("authorization:"));
            }
            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let request_body: serde_json::Value =
                serde_json::from_slice(&request[body_start..]).unwrap();
            assert_eq!(
                request_body["model"], model,
                "preserve the provider's wire model id"
            );
            assert!(!text.contains("unrelated-openai-key"));
            let body = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"package \"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"works\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let mut cmd = command(&temp, &library);
        cmd.env("OPENAI_API_KEY", "unrelated-openai-key").args([
            "--provider",
            provider,
            "--model",
            model,
            "--base-url",
            &base,
            "--print",
            "--no-session",
            "--no-tools",
            "hello",
        ]);
        if auth {
            cmd.env("OPENROUTER_API_KEY", "package-key");
        }
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::task::spawn_blocking(move || cmd.output().unwrap()),
        )
        .await
        .expect("CLI finishes")
        .unwrap();
        assert!(
            output.status.success(),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("package works"),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn reload_updates_the_catalog_endpoint_and_runtime_without_a_restart() {
    let Some(library) = library("rpi_provider_proxy") else {
        return;
    };
    // The child gets an isolated environment; parallel tests never mutate the
    // parent's config directory or credentials.
    if std::env::var_os("RPI_TEST_PACKAGE_RELOAD").is_none() {
        let temp = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "reload_updates_the_catalog_endpoint_and_runtime_without_a_restart",
                "--nocapture",
            ])
            .env("RPI_TEST_PACKAGE_RELOAD", "1")
            .env("RPI_CODING_AGENT_DIR", temp.path().join("agent"))
            .env("RPI_TEST_PACKAGE_CWD", temp.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("PROXY_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let cwd = PathBuf::from(std::env::var_os("RPI_TEST_PACKAGE_CWD").unwrap());
    let args = rpi_cli::args::Args {
        extension: vec![library],
        provider: Some("proxy".into()),
        model: Some("default".into()),
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let prepared = rpi_cli::session::prepare_extensions(&args, &cwd, true).unwrap();
    let resolved =
        rpi_cli::provider::resolve_with_extensions(&args, &cwd, true, &prepared.extension_session)
            .unwrap();
    let (harness, _events, mut context) =
        rpi_cli::session::build_prepared(&resolved, &args, &cwd, true, prepared)
            .await
            .unwrap();
    let before = context
        .extension_session
        .lock()
        .unwrap()
        .snapshot_arc()
        .unwrap();
    let agent = rpi_cli::config::agent_dir().unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("models.json"), r#"{"providers":{"proxy":{"baseUrl":"http://localhost:9012/v1","models":[{"id":"default"},{"id":"second"}]}}}"#).unwrap();
    for _ in 0..2 {
        let outcome = rpi_cli::session::reload_extension_resources(&harness, &context).await;
        assert!(
            !outcome.had_errors,
            "{} {:?}",
            outcome.summary, outcome.details
        );
        assert!(context
            .catalog
            .read()
            .unwrap()
            .iter()
            .any(|model| model.id == "second"));
        let model = harness.lane("main").get_model().await.unwrap();
        assert_eq!(model.base_url, "http://localhost:9012/v1");
        let provider = context
            .providers
            .read()
            .unwrap()
            .iter()
            .find(|provider| provider.id() == "proxy")
            .cloned()
            .unwrap();
        let options = rpi_ai::SimpleStreamOptions::default();
        options.signal.cancel();
        let result = provider
            .stream_simple(&model, &rpi_ai::Context::new(vec![]), &options)
            .await
            .result()
            .await
            .unwrap();
        assert_eq!(
            result.stop_reason,
            rpi_ai::StopReason::Aborted,
            "fresh runtime must not retain a stale snapshot"
        );
    }
    assert!(!before.is_active());
    let before_failed = context
        .extension_session
        .lock()
        .unwrap()
        .snapshot_arc()
        .unwrap();
    context.args.no_extensions = true;
    let outcome = rpi_cli::session::reload_extension_resources(&harness, &context).await;
    assert!(
        outcome.had_errors,
        "removing the active provider should fail cleanly"
    );
    let after_failed = context
        .extension_session
        .lock()
        .unwrap()
        .snapshot_arc()
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&before_failed, &after_failed));
    assert!(after_failed.is_active());
}

#[test]
fn linked_package_is_discovered_and_unlinked_cleanly() {
    if library("rpi_provider_openrouter").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/provider-openrouter");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(temp.path())
            .env("RPI_CODING_AGENT_DIR", temp.path().join("agent"))
            .env("RPI_OFFLINE", "1")
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    };
    let linked = run(&[
        "package",
        "link",
        "openrouter",
        "--path",
        source.to_str().unwrap(),
        "--debug",
        "--locked",
    ]);
    assert!(
        linked.status.success(),
        "{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let catalog = run(&["--list-models", "openrouter"]);
    assert!(catalog.status.success());
    assert!(String::from_utf8_lossy(&catalog.stdout).contains("openrouter/auto"));
    let unlinked = run(&["package", "unlink", "openrouter"]);
    assert!(
        unlinked.status.success(),
        "{}",
        String::from_utf8_lossy(&unlinked.stderr)
    );
    let catalog = run(&["--list-models", "openrouter"]);
    assert!(!String::from_utf8_lossy(&catalog.stdout).contains("openrouter/auto"));
}
