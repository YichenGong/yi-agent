//! Converts natural-language schedule requests into a validated local preview.

use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;
use yi_agent_core::Provider;
use yi_agent_store::schedule::{ScheduleDefinition, ScheduleDefinitionError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulePreview {
    pub definition: ScheduleDefinition,
}

#[derive(Debug, Error)]
pub enum ScheduleIntentError {
    #[error("the schedule request needs clarification")]
    ClarificationRequired,
    #[error(transparent)]
    Provider(#[from] yi_agent_core::ProviderError),
}

#[derive(Deserialize)]
struct ModelIntent {
    cron: String,
    objective: String,
}

pub struct ScheduleIntentParser {
    provider: Arc<dyn Provider>,
    model: String,
}

impl ScheduleIntentParser {
    pub fn new(provider: Arc<dyn Provider>, model: String) -> Self {
        Self { provider, model }
    }

    pub async fn preview(&self, request: &str) -> Result<SchedulePreview, ScheduleIntentError> {
        if request.trim().is_empty() {
            return Err(ScheduleIntentError::ClarificationRequired);
        }
        let prompt = format!(
            "Convert this scheduling request into JSON only. Return exactly {{\"cron\":\"five-field cron\",\"objective\":\"clear task\"}}. \
             Cron is minute hour day-of-month month day-of-week, has no seconds, and uses the daemon machine's local time zone. \
             Do not return policy, permissions, provider settings, limits, explanations, or markdown. If ambiguous, return {{}}.\nRequest: {request}"
        );
        let response = self.provider.complete(&self.model, &prompt).await?;
        let intent: ModelIntent = serde_json::from_str(response.trim())
            .map_err(|_| ScheduleIntentError::ClarificationRequired)?;
        let definition = ScheduleDefinition::new(intent.cron, intent.objective)
            .map_err(schedule_error_to_clarification)?;
        Ok(SchedulePreview { definition })
    }
}

fn schedule_error_to_clarification(_: ScheduleDefinitionError) -> ScheduleIntentError {
    ScheduleIntentError::ClarificationRequired
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use yi_agent_core::{ProviderEvent, ProviderRequest, StopReason};

    struct ScriptedProvider(&'static str);

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn call_stream(
            &self,
            _: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, yi_agent_core::ProviderError> {
            Ok(futures::stream::iter(vec![
                ProviderEvent::TextDelta(self.0.into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ])
            .boxed())
        }
    }

    #[tokio::test]
    async fn preview_validates_model_json_without_persisting_it() {
        let parser = ScheduleIntentParser::new(
            Arc::new(ScriptedProvider(
                r#"{"cron":"0 9 * * 1-5","objective":"Write report"}"#,
            )),
            "test".into(),
        );
        let preview = parser.preview("weekday morning report").await.unwrap();
        assert_eq!(preview.definition.cron, "0 9 * * 1-5");
        assert_eq!(preview.definition.objective, "Write report");
        assert!(preview.definition.policy.runtime.read_only);
    }

    #[tokio::test]
    async fn preview_rejects_ambiguous_or_invalid_model_output() {
        let parser = ScheduleIntentParser::new(Arc::new(ScriptedProvider("{}")), "test".into());
        assert!(matches!(
            parser.preview("sometime tomorrow").await,
            Err(ScheduleIntentError::ClarificationRequired)
        ));
    }
}
