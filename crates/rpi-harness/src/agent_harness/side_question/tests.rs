use super::*;
use crate::session::memory::{InMemorySessionStorage, SystemClock};
use crate::session::session::DefaultIdGenerator;
use crate::session::types::SessionMetadata;
use crate::types::HarnessTool;
use rpi_agent::CollectorEmitter;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};

fn session(id: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: id.into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}
fn emitter() -> Arc<dyn AgentEmitter> {
    Arc::new(CollectorEmitter::new().0)
}

async fn parent(
    script: FauxScript,
) -> (
    AgentHarness,
    Arc<FauxProvider>,
    Arc<rpi_tools::InMemoryExecutionEnv>,
) {
    let provider = FauxProvider::new(script);
    let env = Arc::new(rpi_tools::InMemoryExecutionEnv::new());
    let ctx = rpi_tools::ExecutionToolContext::new(env.clone(), Some(env.clone()));
    let tools = vec![
        HarnessTool::new(rpi_tools::create_read_tool(&ctx, None)),
        HarnessTool::new(rpi_tools::create_write_tool(&ctx)),
    ];
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        tools,
        session: session("parent"),
        system_prompt: Some("project instructions".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    (parent, provider, env)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_question_inherits_private_binding_but_not_main_run_lifetime_or_tools() {
    let (parent, provider, _) = parent(FauxScript::new().with_text("side answer")).await;
    let mut model = provider.default_model().clone();
    model.id = "live-model".into();
    model.base_url = "https://private-endpoint.test".into();
    // Exercise inheritance with a model that can actually enable thinking.
    model.reasoning = true;
    model.headers = Some(BTreeMap::from([(
        "Authorization".into(),
        "private-key".into(),
    )]));
    parent.set_model(model.clone()).await.unwrap();
    parent
        .set_thinking_level(rpi_ai::ThinkingLevel::High)
        .await
        .unwrap();
    let (run_id, _, _) = parent.acquire_run(OperationKind::Run).await.unwrap();
    let parent_tip = parent.get_tip_id().await.unwrap();
    let (child, reference) = parent
        .create_side_question(session("side"), emitter())
        .await
        .unwrap();
    assert!(reference.is_empty());
    let config = child.harness.snapshot_config().unwrap();
    assert_eq!(config.model, model);
    assert!(Arc::ptr_eq(
        &config.models[0],
        &parent.snapshot_config().unwrap().models[0]
    ));
    assert_eq!(config.thinking_level, rpi_ai::ThinkingLevel::High);
    assert!(config.tools.is_empty());
    assert!(config.before_tool_call.is_none());
    assert!(config.transform_context.is_none());
    assert_eq!(child.harness.max_turns, Some(1));
    parent.release_run(&run_id);
    assert!(
        !child.scope.is_cancelled(),
        "main completion must not cancel side questions"
    );
    let result = child.harness.prompt_text("side?", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(parent.get_tip_id().await.unwrap(), parent_tip);
    parent.close().await.unwrap();
    assert!(child.scope.is_cancelled());
}

#[test]
fn side_reference_is_utf8_bounded_and_excludes_non_text_blocks() {
    use rpi_ai::types::{UserContent, UserMessage};
    let messages = vec![
        AgentMessage::User(UserMessage {
            role: rpi_ai::types::UserRole,
            content: UserContent::Text("old-secret-marker".into()),
            timestamp: 0,
        }),
        AgentMessage::User(UserMessage {
            role: rpi_ai::types::UserRole,
            content: UserContent::Text("中".repeat(30_000)),
            timestamp: 0,
        }),
    ];
    let reference = side_question_reference(&messages);
    assert!(reference.len() <= 64 * 1024);
    assert!(!reference.contains("old-secret-marker"));
    assert!(reference.contains("[user]"));
}
