//! Send-safe view of the agent's in-flight work, shared with the leader's auto-update checker and `RelaunchForUpdate` drain.
//! Those are `tokio::spawn` tasks and cannot read the `!Send` `MvpAgent` state on the `LocalSet`.
//!
//! The leader's `agent_busy` flag only counts IPC (Unix-socket) requests.
//! Relay (grok.com WebSocket) traffic is bridged straight into the agent's ACP stdin and never sets it.
//! A relay-driven leader (devbox / remote) therefore always looked idle and got restarted mid-turn on every update.
//! That failure showed up as "Subagent result channel dropped".
//!
//! [`AgentActivity::is_busy`] derives busyness from agent state regardless of transport.
//! [`AgentActivity::flush_all_sessions`] lets the shutdown path end session actors gracefully instead of aborting them via `LocalSet` drop.
//!
//! ## Entries expire with their actor, not with agent bookkeeping
//!
//! The agent only ever **registers** sessions (at handle creation).
//! There is deliberately no unregister: an entry is live exactly while its actor holds the command receiver (`!cmd_tx.is_closed()`).
//! Closed entries are purged whenever the list is locked.
//! This avoids races between `MvpAgent`'s map bookkeeping and the actor's lifetime.
//! An actor removed from the agent's map but still shutting down stays visible to `is_busy`/`flush_all_sessions` until it actually exits.
//! A session id rebuilt with a fresh actor is just a second, distinct entry.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use xai_grok_telemetry::session_end::{self, Phase};

use crate::session::pending_interaction::PendingInteractions;
use crate::session::{SessionCommand, SessionHandle, ShutdownKind};

/// How often [`AgentActivity::flush_all_sessions`] re-polls actors that have not yet exited.
const FLUSH_POLL: Duration = Duration::from_millis(50);

/// Default bound on a process-exit session flush ([`AgentActivity::flush_all_sessions`]).
/// Leader auto-update shutdown and the in-process agent's `/exit` / headless-quit path both use it.
/// One wedged actor therefore delays exit by the same amount everywhere; sessions are normally idle and the flush completes in milliseconds.
///
/// Known gap: a `SessionEnd` hook configured with a longer `timeout` than this is still cut off at the grace.
/// Aligning the two needs the hook registry's configured timeouts at flush time, which this layer does not see.
pub const SESSION_FLUSH_GRACE: Duration = Duration::from_secs(10);

/// Authoritative Agent-wide drain state. Session rows are actor round-trips;
/// subagent/presentation counts are the coordinator's shared gauges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDrainSnapshot {
    pub sessions: Vec<crate::session::commands::SessionDrainSnapshot>,
    pub subagents: usize,
    pub presentations: usize,
    pub unreachable_sessions: Vec<String>,
}

impl AgentDrainSnapshot {
    pub fn queued_prompts(&self) -> usize {
        self.sessions.iter().map(|s| s.queued_prompts).sum()
    }

    pub fn running_prompts(&self) -> usize {
        self.sessions.iter().filter(|s| s.running_prompt).count()
    }

    pub fn outstanding_background_tasks(&self) -> usize {
        self.sessions
            .iter()
            .map(|s| s.outstanding_background_tasks)
            .sum()
    }

    pub fn is_idle(&self) -> bool {
        self.unreachable_sessions.is_empty()
            && self.subagents == 0
            && self.presentations == 0
            && self.sessions.iter().all(|session| session.is_idle())
    }
}

/// Result of fencing and draining an Agent. A timed-out report retains the
/// exact last authoritative snapshot so replacement/shutdown code can refuse
/// a lossy transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuiesceReport {
    pub fence: xai_grok_tools::management::admission::AdmissionSnapshot,
    pub admission: xai_grok_tools::management::admission::AdmissionSnapshot,
    pub initial: AgentDrainSnapshot,
    pub final_snapshot: AgentDrainSnapshot,
    pub polls: u64,
    pub elapsed: Duration,
    pub timed_out: bool,
}

impl QuiesceReport {
    pub fn drained(&self) -> bool {
        !self.timed_out && self.admission.active == 0 && self.final_snapshot.is_idle()
    }

    pub fn rejected_during_quiesce(&self) -> u64 {
        self.admission.rejected.saturating_sub(self.fence.rejected)
    }
}

/// Per-session slice of state shared with the session actor (the same `Arc`s the actor mutates; see the matching `SessionHandle` fields).
struct SessionActivityEntry {
    id: String,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    current_prompt_id: Arc<Mutex<Option<String>>>,
    pending_interactions: PendingInteractions,
    active_work: Arc<AtomicUsize>,
}

impl SessionActivityEntry {
    fn is_live(&self) -> bool {
        !self.cmd_tx.is_closed()
    }

    fn is_busy(&self) -> bool {
        self.current_prompt_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
            || !self
                .pending_interactions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            || self.active_work.load(Ordering::Relaxed) > 0
    }
}

#[derive(Default)]
struct ActivityInner {
    /// Self-expiring: entries are dead once the actor drops its receiver (see module docs), and are purged whenever the list is locked.
    sessions: Mutex<Vec<SessionActivityEntry>>,
    /// Subagents currently initializing or running; kept in sync by the shared coordinator's `running_count_changed` callback.
    subagents: Arc<AtomicUsize>,
    /// Completion presentation can enqueue a related synthetic prompt after
    /// the coordinator's running count reaches zero. This gauge closes that
    /// drain-observation window.
    presentations: AtomicUsize,
    /// The one admission authority shared by every session and scheduler
    /// belonging to this agent.
    admission: xai_grok_tools::management::admission::AdmissionController,
}

/// Cheap-to-clone, `Send + Sync` handle. See module docs.
#[derive(Clone, Default)]
pub struct AgentActivity {
    inner: Arc<ActivityInner>,
}

impl AgentActivity {
    pub(crate) fn admission_controller(
        &self,
    ) -> xai_grok_tools::management::admission::AdmissionController {
        self.inner.admission.clone()
    }

    /// Register a session's shared state at handle-creation time.
    /// No unregister exists; the entry expires when the actor exits.
    pub(crate) fn register_session(&self, id: &str, handle: &SessionHandle) {
        self.lock_live_sessions().push(SessionActivityEntry {
            id: id.to_string(),
            cmd_tx: handle.cmd_tx.clone(),
            current_prompt_id: handle.current_prompt_id.clone(),
            pending_interactions: handle.pending_interactions.clone(),
            active_work: handle.active_work.clone(),
        });
    }

    /// Shared gauge of initializing and running subagents; updated from the shared coordinator's `running_count_changed` callback.
    pub(crate) fn subagent_gauge(&self) -> Arc<AtomicUsize> {
        self.inner.subagents.clone()
    }

    pub(crate) fn begin_presentation(&self) -> PresentationGuard {
        self.inner.presentations.fetch_add(1, Ordering::AcqRel);
        PresentationGuard {
            activity: self.clone(),
        }
    }

    /// Whether the agent has live session, workflow, subagent, or presentation work.
    pub fn is_busy(&self) -> bool {
        self.inner.subagents.load(Ordering::Relaxed) > 0
            || self.inner.presentations.load(Ordering::Acquire) > 0
            || self.lock_live_sessions().iter().any(|e| e.is_busy())
    }

    /// Number of live registered sessions (diagnostics/tests).
    pub fn session_count(&self) -> usize {
        self.lock_live_sessions().len()
    }

    /// Fence all new Agent prompt admissions and wait for every unit accepted
    /// before the fence, its FIFO turn, related session actor work, background
    /// process, subagent, and completion presentation to settle.
    pub async fn quiesce(&self, timeout: Duration) -> QuiesceReport {
        const POLL: Duration = Duration::from_millis(25);
        let started = tokio::time::Instant::now();
        let deadline = started + timeout;
        let fence = self.inner.admission.begin_quiesce();
        let _ = self.inner.admission.wait_for_zero(deadline).await;
        let initial = self.drain_snapshot(deadline).await;
        let mut final_snapshot = initial.clone();
        let mut polls = 1u64;

        loop {
            let admission = self.inner.admission.snapshot();
            if admission.active == 0 && final_snapshot.is_idle() {
                // Completion presenters enqueue their related session command
                // before dropping the presentation gauge. A short quiet
                // barrier followed by another actor round-trip proves that no
                // such command was left behind the first snapshot.
                if tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(POLL).await;
                    final_snapshot = self.drain_snapshot(deadline).await;
                    polls = polls.saturating_add(1);
                }
                let admission = self.inner.admission.snapshot();
                if admission.active == 0 && final_snapshot.is_idle() {
                    let admission = self.inner.admission.mark_quiesced();
                    return QuiesceReport {
                        fence,
                        admission,
                        initial,
                        final_snapshot,
                        polls,
                        elapsed: started.elapsed(),
                        timed_out: false,
                    };
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return QuiesceReport {
                    fence,
                    admission: self.inner.admission.snapshot(),
                    initial,
                    final_snapshot,
                    polls,
                    elapsed: started.elapsed(),
                    timed_out: true,
                };
            }
            tokio::time::sleep(POLL).await;
            final_snapshot = self.drain_snapshot(deadline).await;
            polls = polls.saturating_add(1);
        }
    }

    async fn drain_snapshot(&self, deadline: tokio::time::Instant) -> AgentDrainSnapshot {
        let entries: Vec<_> = self
            .lock_live_sessions()
            .iter()
            .map(|entry| (entry.id.clone(), entry.cmd_tx.clone()))
            .collect();
        let mut pending = FuturesUnordered::new();
        for (id, cmd_tx) in entries {
            pending.push(async move {
                let (respond_to, response) = tokio::sync::oneshot::channel();
                if cmd_tx
                    .send(SessionCommand::GetDrainSnapshot { respond_to })
                    .is_err()
                {
                    return (id, None);
                }
                let snapshot = tokio::time::timeout_at(deadline, response)
                    .await
                    .ok()
                    .and_then(Result::ok);
                (id, snapshot)
            });
        }
        let mut sessions = Vec::new();
        let mut unreachable_sessions = Vec::new();
        while let Some((id, snapshot)) = pending.next().await {
            match snapshot {
                Some(snapshot) => sessions.push(snapshot),
                None => unreachable_sessions.push(id),
            }
        }
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        unreachable_sessions.sort();
        AgentDrainSnapshot {
            sessions,
            subagents: self.inner.subagents.load(Ordering::Acquire),
            presentations: self.inner.presentations.load(Ordering::Acquire),
            unreachable_sessions,
        }
    }

    /// Send [`SessionCommand::Shutdown`] to every live session actor and wait up to `grace` for them to exit, observed via `cmd_tx.is_closed()`.
    /// Shutdown runs the replay-buffer flush, then hooks, then the memory save, then the actor returns.
    ///
    /// This is a quiesce loop, not a one-shot broadcast.
    /// Each poll re-snapshots the registry and signals actors that appeared after the flush started.
    /// Signals are deduped by channel identity, so a session id rebuilt with a fresh actor gets its own signal.
    /// Everything runs against one deadline; `grace` bounds the **total** shutdown delay.
    ///
    /// Callers: the leader's auto-update / `RelaunchForUpdate` shutdown, and the in-process agent worker on `/exit` / headless quit.
    /// In the leader case, call **before** cancelling the root token.
    /// In the in-process case, call **after** the cancel that ends the worker's run loop but before its `LocalSet` drops.
    /// Either way, session state must be durable before the drop aborts remaining tasks.
    /// Actors that miss the grace are logged and abandoned.
    pub async fn flush_all_sessions(&self, grace: Duration) {
        let _ = self.flush_all_sessions_checked(grace).await;
    }

    /// Report whether every actor exited, so embeddings do not report successful
    /// shutdown when native session flushing exceeded its grace.
    pub async fn flush_all_sessions_checked(&self, grace: Duration) -> bool {
        let _span = session_end::span(Phase::SessionFlush);
        let deadline = tokio::time::Instant::now() + grace;
        // Every distinct channel signaled so far (id kept for logging).
        let mut signaled: Vec<(String, tokio::sync::mpsc::UnboundedSender<SessionCommand>)> =
            Vec::new();

        loop {
            let snapshot: Vec<_> = self
                .lock_live_sessions()
                .iter()
                .map(|e| (e.id.clone(), e.cmd_tx.clone()))
                .collect();
            for (id, tx) in snapshot {
                if !signaled.iter().any(|(_, s)| s.same_channel(&tx)) {
                    tracing::info!(session_id = %id, "shutdown: flushing session");
                    let _ = tx.send(SessionCommand::Shutdown(ShutdownKind::Graceful));
                    signaled.push((id, tx));
                }
            }

            if signaled.iter().all(|(_, tx)| tx.is_closed()) {
                return true; // nothing to flush, or all actors exited
            }
            if tokio::time::Instant::now() >= deadline {
                for (id, tx) in &signaled {
                    if !tx.is_closed() {
                        tracing::warn!(
                            session_id = %id,
                            "shutdown: session actor did not exit within grace; proceeding"
                        );
                    }
                }
                return false;
            }
            tokio::time::sleep(FLUSH_POLL).await;
        }
    }

    /// Lock the session list, dropping entries whose actor has exited.
    ///
    /// Purging happens only here, so in modes with no periodic reader (no auto-update checker) a dead entry lingers until the next register.
    /// The leak is bounded and tiny: a sender handle and two `Arc`s per entry.
    fn lock_live_sessions(&self) -> std::sync::MutexGuard<'_, Vec<SessionActivityEntry>> {
        let mut guard = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.retain(SessionActivityEntry::is_live);
        guard
    }

    /// Register a synthetic session from raw parts (no full `SessionHandle`).
    /// Returns the command receiver (the "actor" side) plus the shared running-turn and pending-interaction slots.
    #[cfg(test)]
    pub(crate) fn register_for_test(
        &self,
        id: &str,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
        Arc<Mutex<Option<String>>>,
        PendingInteractions,
    ) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let current_prompt_id = Arc::new(Mutex::new(None));
        let pending_interactions: PendingInteractions =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        self.lock_live_sessions().push(SessionActivityEntry {
            id: id.to_string(),
            cmd_tx,
            current_prompt_id: current_prompt_id.clone(),
            pending_interactions: pending_interactions.clone(),
            active_work: Arc::new(AtomicUsize::new(0)),
        });
        (cmd_rx, current_prompt_id, pending_interactions)
    }
}

pub(crate) struct PresentationGuard {
    activity: AgentActivity,
}

impl Drop for PresentationGuard {
    fn drop(&mut self) {
        self.activity
            .inner
            .presentations
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a registered entry from raw parts without a full SessionHandle.
    fn register_raw(
        activity: &AgentActivity,
        id: &str,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
        Arc<Mutex<Option<String>>>,
        PendingInteractions,
    ) {
        activity.register_for_test(id)
    }

    /// Simulated session actor: exits (dropping its receiver) `delay` after receiving `Shutdown`; resolves to whether Shutdown was received.
    fn spawn_actor(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
        delay: Duration,
    ) -> tokio::task::JoinHandle<bool> {
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                if matches!(cmd, SessionCommand::Shutdown(_)) {
                    tokio::time::sleep(delay).await;
                    return true;
                }
            }
            false
        })
    }

    fn spawn_drain_actor(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
        snapshots: Vec<crate::session::commands::SessionDrainSnapshot>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut snapshots = std::collections::VecDeque::from(snapshots);
            let mut last = crate::session::commands::SessionDrainSnapshot::default();
            while let Some(command) = rx.recv().await {
                if let SessionCommand::GetDrainSnapshot { respond_to } = command {
                    if let Some(snapshot) = snapshots.pop_front() {
                        last = snapshot;
                    }
                    let _ = respond_to.send(last.clone());
                }
            }
        })
    }

    #[test]
    fn idle_by_default() {
        let activity = AgentActivity::default();
        assert!(!activity.is_busy());
    }

    #[tokio::test]
    async fn running_turn_marks_busy() {
        let activity = AgentActivity::default();
        let (_rx, prompt_id, _pending) = register_raw(&activity, "s1");
        assert!(!activity.is_busy());

        *prompt_id.lock().unwrap() = Some("prompt-1".to_string());
        assert!(activity.is_busy());

        *prompt_id.lock().unwrap() = None;
        assert!(!activity.is_busy());
    }

    #[tokio::test]
    async fn pending_interaction_marks_busy() {
        let activity = AgentActivity::default();
        let (_rx, _prompt_id, pending) = register_raw(&activity, "s1");

        pending.lock().unwrap().insert(
            "tc-1".to_string(),
            crate::session::pending_interaction::PendingKind::Permission,
        );
        assert!(activity.is_busy());

        pending.lock().unwrap().clear();
        assert!(!activity.is_busy());
    }

    #[test]
    fn subagent_gauge_marks_busy() {
        let activity = AgentActivity::default();
        let gauge = activity.subagent_gauge();
        assert!(!activity.is_busy());
        gauge.store(1, Ordering::Relaxed);
        assert!(activity.is_busy());
        gauge.store(0, Ordering::Relaxed);
        assert!(!activity.is_busy());
    }

    /// An actor that is still running counts as busy even if the agent has dropped its handle.
    /// Liveness comes from the channel, not from agent bookkeeping; once the actor exits, the entry expires.
    #[tokio::test]
    async fn live_actor_counts_busy_until_it_exits() {
        let activity = AgentActivity::default();
        let (rx, prompt_id, _pending) = register_raw(&activity, "s1");
        *prompt_id.lock().unwrap() = Some("prompt-1".to_string());
        assert!(activity.is_busy());
        assert_eq!(activity.session_count(), 1);

        // The actor exits (receiver dropped), so the entry expires even though the shared prompt slot still says Some
        drop(rx);
        assert!(!activity.is_busy());
        assert_eq!(activity.session_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_sends_shutdown_and_waits_for_actor_exit() {
        let activity = AgentActivity::default();
        let (rx, _prompt_id, _pending) = register_raw(&activity, "s1");

        // Simulated actor: exits (drops rx) when it receives Shutdown.
        let actor = spawn_actor(rx, Duration::ZERO);

        assert!(activity.flush_all_sessions_checked(Duration::from_secs(5)).await);
        assert!(actor.await.unwrap(), "actor should have received Shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn flush_grace_bounds_total_delay_across_sessions() {
        let activity = AgentActivity::default();
        // One wedged actor (receiver kept open) and one healthy actor.
        let (_wedged_rx, _p1, _i1) = register_raw(&activity, "wedged");
        let (rx, _p2, _i2) = register_raw(&activity, "healthy");
        let actor = spawn_actor(rx, Duration::ZERO);

        // The wedged actor must not consume the healthy actor's budget
        // The total wait must be about one grace period, not one per session
        let start = tokio::time::Instant::now();
        activity.flush_all_sessions(Duration::from_secs(2)).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_secs(2));
        assert!(
            elapsed < Duration::from_secs(3),
            "grace must be shared, not serial: {elapsed:?}"
        );
        assert!(actor.await.unwrap(), "healthy actor should get Shutdown");
    }

    /// A session id rebuilt with a fresh actor while the old actor is still shutting down: both channels must be signaled and awaited.
    #[tokio::test(start_paused = true)]
    async fn flush_awaits_both_channels_when_id_is_reused() {
        let activity = AgentActivity::default();
        let (old_rx, _p1, _i1) = register_raw(&activity, "s1");
        let (new_rx, _p2, _i2) = register_raw(&activity, "s1");

        let old_actor = spawn_actor(old_rx, Duration::from_millis(500));
        let new_actor = spawn_actor(new_rx, Duration::ZERO);

        activity.flush_all_sessions(Duration::from_secs(5)).await;
        assert!(old_actor.is_finished(), "flush must wait for the old actor");
        assert!(old_actor.await.unwrap());
        assert!(new_actor.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn flush_signals_sessions_that_appear_mid_flush() {
        let activity = AgentActivity::default();
        // Actor 1: holds the flush open for a few polls, then exits.
        let (rx1, _p1, _i1) = register_raw(&activity, "s1");
        let actor1 = spawn_actor(rx1, Duration::from_millis(300));

        // Actor 2 registers AFTER the flush has started (a relay-driven prompt racing the shutdown); it must still receive Shutdown
        let activity_late = activity.clone();
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (mut rx2, _p2, _i2) = activity_late.register_for_test("s2");
            while let Some(cmd) = rx2.recv().await {
                if matches!(cmd, SessionCommand::Shutdown(_)) {
                    return true;
                }
            }
            false
        });

        activity.flush_all_sessions(Duration::from_secs(5)).await;
        assert!(actor1.await.unwrap());
        assert!(
            late.await.unwrap(),
            "session registered mid-flush must receive Shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flush_gives_up_after_grace_when_actor_hangs() {
        let activity = AgentActivity::default();
        // Keep rx alive so the channel never closes (wedged actor).
        let (_rx, _prompt_id, _pending) = register_raw(&activity, "s1");

        let start = tokio::time::Instant::now();
        assert!(!activity.flush_all_sessions_checked(Duration::from_secs(2)).await);
        assert!(
            start.elapsed() >= Duration::from_secs(2),
            "flush should wait out the grace period"
        );
        // Returned rather than hanging forever; that's the assertion
    }

    #[tokio::test]
    async fn flush_with_no_sessions_is_noop() {
        let activity = AgentActivity::default();
        activity.flush_all_sessions(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn quiesce_reports_actor_fifo_drain_and_rejects_old_session_authority() {
        let activity = AgentActivity::default();
        let old_session_admission = activity.admission_controller();
        let (rx, _prompt_id, _pending) = register_raw(&activity, "s1");
        let actor = spawn_drain_actor(
            rx,
            vec![
                crate::session::commands::SessionDrainSnapshot {
                    session_id: "s1".into(),
                    queued_prompts: 2,
                    running_prompt: true,
                    pending_interactions: 0,
                    outstanding_background_tasks: 1,
                    active_work: 1,
                },
                crate::session::commands::SessionDrainSnapshot {
                    session_id: "s1".into(),
                    ..Default::default()
                },
            ],
        );

        let report = activity.quiesce(Duration::from_secs(1)).await;

        assert!(report.drained());
        assert_eq!(report.initial.queued_prompts(), 2);
        assert_eq!(report.initial.running_prompts(), 1);
        assert_eq!(report.initial.outstanding_background_tasks(), 1);
        assert!(report.final_snapshot.is_idle());
        let rejection = old_session_admission
            .try_admit(xai_grok_tools::management::admission::AdmissionSource::Human)
            .expect_err("a pre-fence session authority must reject after quiesce");
        assert_eq!(
            rejection.state,
            xai_grok_tools::management::admission::AdmissionState::Quiesced
        );
        actor.abort();
    }

    #[tokio::test]
    async fn quiesce_waits_for_workflow_between_turns() {
        let activity = AgentActivity::default();
        let (rx, _prompt_id, _pending) = register_raw(&activity, "workflow");
        let actor = spawn_drain_actor(
            rx,
            vec![
                crate::session::commands::SessionDrainSnapshot {
                    session_id: "workflow".into(),
                    active_work: 1,
                    ..Default::default()
                },
                crate::session::commands::SessionDrainSnapshot {
                    session_id: "workflow".into(),
                    ..Default::default()
                },
            ],
        );
        let report = activity.quiesce(Duration::from_secs(1)).await;
        assert!(!report.initial.is_idle());
        assert_eq!(report.initial.running_prompts(), 0);
        assert!(report.drained());
        assert!(report.polls > 1);
        actor.abort();
    }

    #[tokio::test]
    async fn quiesce_times_out_on_workflow_without_a_running_prompt() {
        let activity = AgentActivity::default();
        let (rx, _prompt_id, _pending) = register_raw(&activity, "workflow");
        let actor = spawn_drain_actor(
            rx,
            vec![crate::session::commands::SessionDrainSnapshot {
                session_id: "workflow".into(),
                active_work: 1,
                ..Default::default()
            }],
        );
        let report = activity.quiesce(Duration::from_millis(100)).await;
        assert!(report.timed_out);
        assert!(!report.drained());
        assert!(!report.final_snapshot.is_idle());
        actor.abort();
    }
}
