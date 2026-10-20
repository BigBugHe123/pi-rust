//! Reverse runtime bindings must not own the harness's tools/providers.
use super::*;

/// A runtime-action binding that cannot form a cycle through plugin keepalives.
/// Upgrade pins a complete harness for one operation. Session and event facades
/// remain shared, so switching sessions/lanes keeps the same semantics.
#[derive(Clone)]
pub struct WeakAgentHarness {
    session: Session,
    inner: std::sync::Weak<Mutex<HarnessInner>>,
    bus: HarnessEventBus,
    lane: String,
    message_writes: Arc<tokio::sync::Mutex<()>>,
    side_question_scope: Arc<Mutex<CancellationToken>>,
    is_side_question: bool,
    max_turns: Option<u32>,
}
impl AgentHarness {
    pub fn downgrade(&self) -> WeakAgentHarness {
        WeakAgentHarness {
            session: self.session.clone(),
            inner: Arc::downgrade(&self.inner),
            bus: self.bus.clone(),
            lane: self.lane.clone(),
            message_writes: self.message_writes.clone(),
            side_question_scope: self.side_question_scope.clone(),
            is_side_question: self.is_side_question,
            max_turns: self.max_turns,
        }
    }
}
impl WeakAgentHarness {
    pub fn upgrade(&self) -> Option<AgentHarness> {
        Some(AgentHarness {
            session: self.session.clone(),
            inner: self.inner.upgrade()?,
            bus: self.bus.clone(),
            lane: self.lane.clone(),
            message_writes: self.message_writes.clone(),
            side_question_scope: self.side_question_scope.clone(),
            is_side_question: self.is_side_question,
            max_turns: self.max_turns,
        })
    }
}
impl Drop for HarnessInner {
    fn drop(&mut self) {
        self.side_question_scope
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel();
        if let Some(active) = &self.active_run {
            active.signal.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_weak_runtime_binding_does_not_retain_the_last_owner_or_side_questions() {
        let parent = AgentHarness::create(AgentHarnessOptions::default())
            .await
            .unwrap();
        let scope = parent.side_question_scope.lock().unwrap().child_token();
        let weak = parent.downgrade();
        let clone = weak.upgrade().unwrap();
        drop(parent);
        assert!(weak.upgrade().is_some());
        assert!(!scope.is_cancelled());
        drop(clone);
        assert!(weak.upgrade().is_none());
        assert!(scope.is_cancelled());
    }
}
