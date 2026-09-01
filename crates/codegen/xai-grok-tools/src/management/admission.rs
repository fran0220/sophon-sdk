//! Agent-wide prompt-admission fence.
//!
//! One controller is owned by an agent and cloned into every session actor and
//! scheduler actor belonging to it. `try_admit` and `begin_quiesce` take the
//! same lock, which is the linearization point: after `begin_quiesce` returns,
//! no human, peer, or scheduler prompt can acquire a permit.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// Lifecycle of the agent-wide prompt-admission fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionState {
    Open,
    Quiescing,
    Quiesced,
}

/// Authority asking to admit a new root unit of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionSource {
    Human,
    Peer,
    Scheduler,
}

/// Monotonic state and counters for an admission controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    pub generation: u64,
    pub state: AdmissionState,
    pub active: u64,
    pub accepted: u64,
    pub rejected: u64,
}

/// Structured rejection returned when the fence is no longer open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("agent admission is {state:?} at generation {generation}")]
pub struct AdmissionRejection {
    pub generation: u64,
    pub state: AdmissionState,
    pub admission_source: AdmissionSource,
}

#[derive(Debug)]
struct ControllerState {
    generation: u64,
    state: AdmissionState,
    active: u64,
    accepted: u64,
    rejected: u64,
}

#[derive(Debug)]
struct ControllerInner {
    state: Mutex<ControllerState>,
    changed: Notify,
}

/// Cheap-to-clone handle to the one authoritative admission fence for an
/// agent runtime.
#[derive(Debug, Clone)]
pub struct AdmissionController {
    inner: Arc<ControllerInner>,
}

impl Default for AdmissionController {
    fn default() -> Self {
        Self {
            inner: Arc::new(ControllerInner {
                state: Mutex::new(ControllerState {
                    generation: 0,
                    state: AdmissionState::Open,
                    active: 0,
                    accepted: 0,
                    rejected: 0,
                }),
                changed: Notify::new(),
            }),
        }
    }
}

impl AdmissionController {
    /// Atomically admit work while the fence is open.
    ///
    /// The returned permit is cloneable, but contributes exactly one active
    /// admission until the last clone is dropped. Actors keep it with the
    /// authoritative queued/running work so a quiescer cannot observe a gap
    /// between admission and queue insertion.
    pub fn try_admit(
        &self,
        source: AdmissionSource,
    ) -> Result<AdmissionPermit, AdmissionRejection> {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.state != AdmissionState::Open {
            state.rejected = state.rejected.saturating_add(1);
            let rejection = AdmissionRejection {
                generation: state.generation,
                state: state.state,
                admission_source: source,
            };
            drop(state);
            self.inner.changed.notify_waiters();
            return Err(rejection);
        }
        state.active = state.active.saturating_add(1);
        state.accepted = state.accepted.saturating_add(1);
        let generation = state.generation;
        drop(state);
        self.inner.changed.notify_waiters();
        Ok(AdmissionPermit {
            inner: Arc::new(PermitInner {
                controller: self.clone(),
                generation,
                source,
            }),
        })
    }

    /// Close the fence. This operation is idempotent and shares its
    /// linearization lock with [`Self::try_admit`].
    pub fn begin_quiesce(&self) -> AdmissionSnapshot {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.state == AdmissionState::Open {
            state.generation = state.generation.saturating_add(1);
            state.state = AdmissionState::Quiescing;
        }
        let snapshot = snapshot(&state);
        drop(state);
        self.inner.changed.notify_waiters();
        snapshot
    }

    /// Mark a successfully drained fence quiesced.
    pub fn mark_quiesced(&self) -> AdmissionSnapshot {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.state == AdmissionState::Quiescing && state.active == 0 {
            state.state = AdmissionState::Quiesced;
        }
        let snapshot = snapshot(&state);
        drop(state);
        self.inner.changed.notify_waiters();
        snapshot
    }

    pub fn snapshot(&self) -> AdmissionSnapshot {
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        snapshot(&state)
    }

    /// Wait until every accepted permit has settled, bounded by `deadline`.
    pub async fn wait_for_zero(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            let notified = self.inner.changed.notified();
            if self.snapshot().active == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.snapshot().active == 0;
            }
        }
    }
}

fn snapshot(state: &ControllerState) -> AdmissionSnapshot {
    AdmissionSnapshot {
        generation: state.generation,
        state: state.state,
        active: state.active,
        accepted: state.accepted,
        rejected: state.rejected,
    }
}

/// Proof that one root unit of work was accepted before the fence closed.
/// The active count is decremented when the last clone is dropped.
#[derive(Clone)]
pub struct AdmissionPermit {
    inner: Arc<PermitInner>,
}

impl std::fmt::Debug for AdmissionPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionPermit")
            .field("generation", &self.inner.generation)
            .field("source", &self.inner.source)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct PermitInner {
    controller: AdmissionController,
    generation: u64,
    source: AdmissionSource,
}

impl Drop for PermitInner {
    fn drop(&mut self) {
        let mut state = self
            .controller
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        debug_assert!(state.active > 0, "admission permit count underflow");
        state.active = state.active.saturating_sub(1);
        drop(state);
        self.controller.inner.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiesce_linearizes_with_admission_and_rejects_after_fence() {
        let controller = AdmissionController::default();
        let permit = controller.try_admit(AdmissionSource::Human).unwrap();
        let fenced = controller.begin_quiesce();
        assert_eq!(fenced.state, AdmissionState::Quiescing);
        assert_eq!(fenced.active, 1);

        let rejection = controller
            .try_admit(AdmissionSource::Scheduler)
            .unwrap_err();
        assert_eq!(rejection.generation, fenced.generation);
        assert_eq!(rejection.state, AdmissionState::Quiescing);
        assert_eq!(controller.snapshot().rejected, 1);

        drop(permit);
        assert_eq!(controller.mark_quiesced().state, AdmissionState::Quiesced);
    }

    #[tokio::test]
    async fn cloned_permit_counts_once_and_waits_for_last_clone() {
        let controller = AdmissionController::default();
        let permit = controller.try_admit(AdmissionSource::Peer).unwrap();
        let clone = permit.clone();
        controller.begin_quiesce();
        drop(permit);
        assert_eq!(controller.snapshot().active, 1);
        drop(clone);
        assert!(controller.wait_for_zero(tokio::time::Instant::now()).await);
    }

    #[test]
    fn concurrent_quiesce_and_prompt_have_one_linearized_outcome() {
        for _ in 0..128 {
            let controller = AdmissionController::default();
            let prompt_controller = controller.clone();
            let fence_controller = controller.clone();
            let start = Arc::new(std::sync::Barrier::new(3));

            let prompt_start = start.clone();
            let prompt = std::thread::spawn(move || {
                prompt_start.wait();
                prompt_controller.try_admit(AdmissionSource::Human)
            });
            let fence_start = start.clone();
            let fence = std::thread::spawn(move || {
                fence_start.wait();
                fence_controller.begin_quiesce()
            });
            start.wait();

            let admission = prompt.join().unwrap();
            let fenced = fence.join().unwrap();
            match admission {
                Ok(permit) => {
                    assert_eq!(fenced.active, 1);
                    assert_eq!(fenced.accepted, 1);
                    drop(permit);
                }
                Err(rejection) => {
                    assert_eq!(rejection.generation, fenced.generation);
                    assert_eq!(rejection.state, AdmissionState::Quiescing);
                    assert_eq!(fenced.active, 0);
                    assert_eq!(fenced.accepted, 0);
                }
            }
            assert_eq!(controller.mark_quiesced().state, AdmissionState::Quiesced);
        }
    }
}
