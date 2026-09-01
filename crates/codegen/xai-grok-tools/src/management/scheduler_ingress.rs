//! Typed scheduler-to-session prompt ingress.

use std::sync::Arc;

use super::admission::AdmissionPermit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerPrompt {
    pub task_id: String,
    pub prompt: String,
    pub human_schedule: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SchedulerIngressError {
    #[error("owning session actor is unavailable")]
    SessionUnavailable,
}

type IngressFn =
    dyn Fn(SchedulerPrompt, AdmissionPermit) -> Result<(), SchedulerIngressError> + Send + Sync;

/// Host-provided, typed ingress used by the scheduler actor for foreground
/// fires. The admission permit is transferred into the session mailbox, so
/// quiesce cannot observe a gap between scheduler admission and actor intake.
#[derive(Clone)]
pub struct SchedulerPromptIngress(Arc<IngressFn>);

impl SchedulerPromptIngress {
    pub fn new(
        ingress: impl Fn(SchedulerPrompt, AdmissionPermit) -> Result<(), SchedulerIngressError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self(Arc::new(ingress))
    }

    pub fn enqueue(
        &self,
        prompt: SchedulerPrompt,
        permit: AdmissionPermit,
    ) -> Result<(), SchedulerIngressError> {
        (self.0)(prompt, permit)
    }
}

impl std::fmt::Debug for SchedulerPromptIngress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerPromptIngress")
            .finish_non_exhaustive()
    }
}
