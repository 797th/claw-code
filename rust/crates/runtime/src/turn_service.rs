use std::path::PathBuf;

use crate::conversation::{ApiClient, AutoCompactionEvent, ConversationRuntime, RuntimeError};
use crate::dreamer::{maybe_run_auto_dream, DreamRun};
use crate::permissions::PermissionPrompter;
use crate::session::{ContentBlock, Session};
use crate::usage::TokenUsage;
use crate::{MemoryConfig, MemoryManager, ToolExecutor};

/// Outcome of dream handling after a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnDreamStatus {
    Disabled,
    Skipped,
    Completed(DreamRun),
    Failed(String),
}

/// Shared result shape for CLI and future chat transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnServiceOutput {
    pub final_text: String,
    pub session_id: String,
    pub session_path: Option<PathBuf>,
    pub usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
    pub dream_status: TurnDreamStatus,
}

/// Thin shared runtime service around [`ConversationRuntime`].
///
/// Transport-specific shells are expected to own provider construction,
/// permission prompting, and session reference mapping, then call this service
/// for the actual turn lifecycle.
pub struct TurnService<C, T> {
    runtime: ConversationRuntime<C, T>,
    cwd: PathBuf,
    memory_config: MemoryConfig,
}

impl<C, T> TurnService<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        runtime: ConversationRuntime<C, T>,
        cwd: impl Into<PathBuf>,
        memory_config: MemoryConfig,
    ) -> Self {
        Self {
            runtime,
            cwd: cwd.into(),
            memory_config,
        }
    }

    pub fn run_turn(
        &mut self,
        user_text: impl Into<String>,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnServiceOutput, RuntimeError> {
        let summary = self.runtime.run_turn(user_text, prompter)?;
        let dream_status = self.maybe_dream_after_turn();
        let session = self.runtime.session();
        Ok(TurnServiceOutput {
            final_text: final_assistant_text_from_blocks(&summary.assistant_messages),
            session_id: session.session_id.clone(),
            session_path: session.persistence_path().map(PathBuf::from),
            usage: summary.usage,
            auto_compaction: summary.auto_compaction,
            dream_status,
        })
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        self.runtime.session()
    }

    #[must_use]
    pub fn runtime(&self) -> &ConversationRuntime<C, T> {
        &self.runtime
    }

    pub fn runtime_mut(&mut self) -> &mut ConversationRuntime<C, T> {
        &mut self.runtime
    }

    #[must_use]
    pub fn into_runtime(self) -> ConversationRuntime<C, T> {
        self.runtime
    }

    fn maybe_dream_after_turn(&mut self) -> TurnDreamStatus {
        if !self.memory_config.auto_dream_enabled() {
            return TurnDreamStatus::Disabled;
        }
        let manager = MemoryManager::new(self.cwd.clone(), self.memory_config.clone());
        match maybe_run_auto_dream(
            &manager.dream_config(),
            &self.cwd,
            true,
            self.runtime.api_client_mut(),
        ) {
            Ok(Some(run)) => TurnDreamStatus::Completed(run),
            Ok(None) => TurnDreamStatus::Skipped,
            Err(error) => TurnDreamStatus::Failed(error.to_string()),
        }
    }
}

fn final_assistant_text_from_blocks(messages: &[crate::session::ConversationMessage]) -> String {
    messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{ApiRequest, AssistantEvent, StaticToolExecutor};
    use crate::permissions::{PermissionMode, PermissionPolicy};
    use crate::session::Session;

    struct StubClient;

    impl ApiClient for StubClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(vec![
                AssistantEvent::TextDelta("done".to_string()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    #[test]
    fn turn_service_returns_final_text_session_usage_and_dream_status() {
        let runtime = ConversationRuntime::new(
            Session::new(),
            StubClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        let mut service = TurnService::new(runtime, std::env::temp_dir(), MemoryConfig::default());

        let output = service.run_turn("hello", None).expect("turn should run");

        assert_eq!(output.final_text, "done");
        assert_eq!(output.dream_status, TurnDreamStatus::Disabled);
        assert!(output.session_id.starts_with("session-"));
    }
}
