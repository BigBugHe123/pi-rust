//! Independent, parent-owned execution snapshots. No credentials cross JSON.
use super::*;

/// A child runner and the parent cancellation signals it must observe.
/// The host owns its scheduling, deadline and output limits.
pub struct ChildHarness {
    pub harness: AgentHarness,
    pub scope: CancellationToken,
    pub parent_signal: Option<CancellationToken>,
    pub execution_scope: Arc<rpi_agent::tool_execution_scope::ToolExecutionScope>,
}

struct ChildTool {
    tool: Arc<dyn rpi_agent::AgentTool>,
    scope: Arc<rpi_agent::tool_execution_scope::ToolExecutionScope>,
}
#[async_trait::async_trait]
impl rpi_agent::AgentTool for ChildTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        self.tool.schema()
    }
    fn label(&self) -> &str {
        self.tool.label()
    }
    fn execution_mode(&self) -> rpi_agent::ToolExecutionMode {
        self.tool.execution_mode()
    }
    fn prepare_arguments(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, rpi_agent::AgentError> {
        self.tool.prepare_arguments(args)
    }
    async fn execute(
        &self,
        id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        self.scope.run(async {
            // Agent tool batches may detach their Tokio task when the run is
            // dropped. Track that task as well as any foreign driver it starts.
            let _task = rpi_agent::tool_execution_scope::track_background_tool();
            tokio::select! {
                biased;
                _ = signal.cancelled() => Err(rpi_agent::AgentError::Tool("child tool cancelled".into())),
                result = self.tool.execute(id, params, signal.child_token(), update) => result,
            }
        }).await
    }
}

impl AgentHarness {
    /// Public metadata deliberately excludes endpoints, headers and provider bindings.
    pub fn execution_context(&self) -> HarnessResult<serde_json::Value> {
        let snap = self.snapshot_config()?;
        let tools: Vec<_> = Self::active_tools(&snap.tools, &snap.active_tool_names)
            .iter()
            .map(|tool| tool.schema().name.clone())
            .collect();
        Ok(serde_json::json!({
            "model": {"id": snap.model.id, "provider": snap.model.provider, "api": snap.model.api},
            "thinkingLevel": snap.thinking_level,
            "tools": tools,
            "childAgent": !self.is_child,
        }))
    }

    /// Snapshot the *effective* parent execution binding, not CLI defaults.
    /// Tools retain the parent's project environment, extension owner/context and
    /// approval hooks. Only transcript, emitter, admission and turn budget differ.
    /// Requested tools may narrow, never expand, the parent's current capabilities.
    pub async fn create_child(
        &self,
        session: Session,
        emitter: Arc<dyn AgentEmitter>,
        max_turns: u32,
        requested_tools: Option<Vec<String>>,
    ) -> HarnessResult<ChildHarness> {
        if self.is_child || rpi_agent::tool_execution_scope::is_child_execution() {
            return Err(HarnessError::io("recursive child agents are not allowed"));
        }
        if !(1..=32).contains(&max_turns) {
            return Err(HarnessError::io("child maxTurns must be between 1 and 32"));
        }
        if session.storage().metadata().id == self.session.storage().metadata().id {
            return Err(HarnessError::io(
                "child session must be independent of the parent",
            ));
        }
        let snap = self.snapshot_config()?;
        let mut tools: Vec<_> = snap
            .tools
            .iter()
            .filter(|tool| {
                let name = &tool.tool.schema().name;
                name != "delegate_task"
                    && (snap.active_tool_names.is_empty() || snap.active_tool_names.contains(name))
            })
            .cloned()
            .collect();
        if let Some(names) = requested_tools {
            if names
                .iter()
                .any(|name| !tools.iter().any(|tool| tool.tool.schema().name == *name))
            {
                return Err(HarnessError::io(
                    "child tools must be a subset of active parent tools (delegation excluded)",
                ));
            }
            tools.retain(|tool| names.contains(&tool.tool.schema().name));
        }
        let execution_scope = Arc::new(
            rpi_agent::tool_execution_scope::ToolExecutionScope::with_context(serde_json::json!({
                "model": {"id":snap.model.id,"provider":snap.model.provider,"api":snap.model.api},
                "thinkingLevel":snap.thinking_level,
                "tools":tools.iter().map(|tool| tool.tool.schema().name.clone()).collect::<Vec<_>>(),
                "childAgent":false,
            })),
        );
        for tool in &mut tools {
            tool.tool = Arc::new(ChildTool {
                tool: tool.tool.clone(),
                scope: execution_scope.clone(),
            });
        }
        let active_tool_names = tools
            .iter()
            .map(|tool| tool.tool.schema().name.clone())
            .collect();
        let scope = snap.child_scope;
        let parent_signal = snap.parent_signal;
        let steering_mode = snap.steering_queue.lock().unwrap().mode;
        let follow_up_mode = snap.follow_up_queue.lock().unwrap().mode;
        let mut child = Self::create(AgentHarnessOptions {
            model: snap.model,
            thinking_level: snap.thinking_level,
            tools,
            active_tool_names,
            system_prompt: snap.system_prompt,
            resources: snap.resources,
            stream_options: snap.stream_options,
            retry: snap.retry,
            compaction: snap.compaction,
            tool_execution: snap.tool_execution,
            drive: snap.driving,
            steering_mode,
            follow_up_mode,
            models: snap.models,
            to_provider_messages: snap.to_provider_messages,
            entry_projectors: snap.entry_projectors,
            entry_transforms: snap.entry_transforms,
            before_tool_call: snap.before_tool_call,
            after_tool_call: snap.after_tool_call,
            transform_context: snap.transform_context,
            provider_hooks: snap.provider_hooks,
            agent_emitter: Some(emitter),
            session,
            ..Default::default()
        })
        .await?;
        child.is_child = true;
        child.max_turns = Some(max_turns);
        child.inner.lock().unwrap().execution_scope = Some(execution_scope.clone());
        Ok(ChildHarness {
            harness: child,
            scope,
            parent_signal,
            execution_scope,
        })
    }
}

#[cfg(test)]
mod tests;
