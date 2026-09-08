use super::*;
use crate::session::portability::{PortabilityError, PortableImportStatus, PortableSession};

impl MvpAgent {
    /// Capture while leaving the same session actor resident and usable.
    /// Conservatively reserves agent-wide admission: no accepted root work
    /// may straddle the capture, including scheduler/peer work.
    pub async fn export_portable(&self, id: &str) -> Result<PortableSession, PortabilityError> {
        self.capture_portable(id, None).await
    }

    /// Deliver a unique projection handoff marker before releasing admission.
    pub async fn capture_portable(
        &self,
        id: &str,
        history_boundary: Option<String>,
    ) -> Result<PortableSession, PortabilityError> {
        let fence = self
            .activity
            .admission_controller()
            .try_exclusive()
            .ok_or(PortabilityError::Busy)?;
        if self.activity.is_busy() || self.session_registry.attaching_count() != 0 {
            return Err(PortabilityError::Busy);
        }
        let id = acp::SessionId::new(id.to_owned());
        let handle = self
            .get_session_handle(&id)
            .ok_or(PortabilityError::Unavailable)?;
        if let Some(scheduler) = &handle.scheduler_handle {
            let snapshot = scheduler.snapshot().await.map_err(|_| {
                PortabilityError::Incomplete("scheduler inventory unavailable".into())
            })?;
            if !snapshot.tasks.is_empty() {
                return Err(PortabilityError::Incomplete(
                    "scheduled tasks are not portable".into(),
                ));
            }
        }
        let (respond_to, response) = oneshot::channel();
        handle
            .cmd_tx
            .send(SessionCommand::ExportPortable {
                fence,
                history_boundary,
                respond_to,
            })
            .map_err(|_| PortabilityError::Unavailable)?;
        response.await.map_err(|_| PortabilityError::Unavailable)?
    }

    /// Create-only original-ID import. No actor is constructed and no tools,
    /// scheduled actions or workflows are started. Explicitly attach afterward
    /// using local model/permission/MCP configuration.
    pub fn import_portable(
        &self,
        snapshot: &PortableSession,
        cwd: &std::path::Path,
    ) -> Result<String, PortabilityError> {
        let id = acp::SessionId::new(snapshot.session_id().to_owned());
        if self.activity.has_live_session(snapshot.session_id())
            || self.session_registry.is_attaching(&id)
        {
            return Err(PortabilityError::ActiveSession(
                snapshot.session_id().into(),
            ));
        }
        let _fence = self
            .activity
            .admission_controller()
            .try_exclusive()
            .ok_or(PortabilityError::Busy)?;
        if self.activity.is_busy() || self.session_registry.attaching_count() != 0 {
            return Err(PortabilityError::Busy);
        }
        // No await between registry check and atomic publication; native
        // attach/new admission on this MvpAgent's LocalSet cannot interleave.
        crate::session::portability::import(snapshot, cwd)
    }

    /// Inspect an uncertain import without attaching/replaying or trusting a
    /// stale receipt. Comparison normalizes only destination-local metadata.
    pub fn portable_import_status(
        &self,
        snapshot: &PortableSession,
        cwd: &std::path::Path,
    ) -> Result<PortableImportStatus, PortabilityError> {
        let id = acp::SessionId::new(snapshot.session_id().to_owned());
        if self.activity.has_live_session(snapshot.session_id())
            || self.session_registry.is_attaching(&id)
        {
            return Err(PortabilityError::ActiveSession(
                snapshot.session_id().into(),
            ));
        }
        let _fence = self
            .activity
            .admission_controller()
            .try_exclusive()
            .ok_or(PortabilityError::Busy)?;
        if self.activity.is_busy() || self.session_registry.attaching_count() != 0 {
            return Err(PortabilityError::Busy);
        }
        crate::session::portability::import_status(snapshot, cwd)
    }
}
