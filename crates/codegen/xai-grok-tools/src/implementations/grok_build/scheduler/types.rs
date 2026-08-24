use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SchedulerVersion {
    generation: uuid::Uuid,
    revision: u64,
}

impl SchedulerVersion {
    pub(super) fn generation(self) -> String {
        self.generation.to_string()
    }

    pub(super) fn revision(self) -> u64 {
        self.revision
    }

    pub(super) fn generation_id(self) -> uuid::Uuid {
        self.generation
    }

    #[cfg(test)]
    pub(super) fn from_parts(generation: uuid::Uuid, revision: u64) -> Self {
        Self {
            generation,
            revision,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SchedulerClock {
    version: SchedulerVersion,
}

#[derive(Debug)]
pub(crate) struct SchedulerReservation {
    source: SchedulerVersion,
    generation: uuid::Uuid,
    next_revision: u64,
    remaining: u64,
    rollover: Option<GenerationRollover>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationRollover {
    pub(crate) old_generation: uuid::Uuid,
    pub(crate) new_generation: uuid::Uuid,
}

pub(crate) struct SchedulerCommit {
    pub(crate) version: SchedulerVersion,
    pub(crate) rollover: Option<GenerationRollover>,
}

impl SchedulerReservation {
    pub(crate) fn version_at(&self, offset: u64) -> SchedulerVersion {
        assert!(
            offset < self.remaining,
            "scheduler reservation offset is exhausted"
        );
        SchedulerVersion {
            generation: self.generation,
            revision: self.next_revision + offset,
        }
    }

    pub(crate) fn commit_next(&mut self, clock: &mut SchedulerClock) -> SchedulerCommit {
        assert!(self.remaining > 0, "scheduler reservation is exhausted");
        let rollover = self.rollover;
        let expected_source = rollover.map_or(
            SchedulerVersion {
                generation: self.generation,
                revision: self.next_revision - 1,
            },
            |_| self.source,
        );
        assert_eq!(
            clock.version, expected_source,
            "stale scheduler reservation"
        );

        let version = self.version_at(0);
        clock.version = version;
        self.rollover = None;
        self.remaining -= 1;
        if self.remaining > 0 {
            self.next_revision = self
                .next_revision
                .checked_add(1)
                .expect("preflighted revision");
        }
        SchedulerCommit { version, rollover }
    }
}

impl SchedulerClock {
    pub(crate) fn new() -> Self {
        Self {
            version: SchedulerVersion {
                generation: uuid::Uuid::now_v7(),
                revision: 0,
            },
        }
    }

    pub(crate) fn snapshot(&self) -> SchedulerVersion {
        self.version
    }

    pub(crate) fn prepare_transition(&self, count: usize) -> SchedulerReservation {
        assert!(
            count > 0 && count <= MAX_SCHEDULER_TRANSITIONS,
            "invalid scheduler reservation size"
        );
        let count = count as u64;
        let rollover =
            self.version
                .revision
                .checked_add(count)
                .is_none()
                .then(|| GenerationRollover {
                    old_generation: self.version.generation,
                    new_generation: uuid::Uuid::now_v7(),
                });
        SchedulerReservation {
            source: self.version,
            generation: rollover
                .map(|rollover| rollover.new_generation)
                .unwrap_or(self.version.generation),
            next_revision: if rollover.is_some() {
                1
            } else {
                self.version.revision + 1
            },
            remaining: count,
            rollover,
        }
    }

    #[cfg(test)]
    pub(super) fn at_revision_for_test(revision: u64) -> Self {
        let mut clock = Self::new();
        clock.version.revision = revision;
        clock
    }
}

impl Default for SchedulerClock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum SchedulerError {
    #[error("invalid interval: {0}")]
    InvalidInterval(String),

    #[error("maximum of {0} scheduled tasks reached")]
    TaskLimitReached(usize),

    #[error("no scheduled task with id {0}; call scheduler_list to see active task ids")]
    TaskNotFound(String),

    #[error("scheduled task {0} does not accept host-delivered occurrences")]
    DeliveryUnsupported(String),

    #[error("invalid scheduled occurrence: {0}")]
    InvalidOccurrence(String),

    #[error("cannot replace wake source for scheduled task {0} while occurrences are pending")]
    WakeSourceUpdatePending(String),

    #[error("failed to persist scheduler resources: {0}")]
    Persistence(#[source] std::io::Error),

    #[error("scheduler durability outcome is unknown: {0}")]
    DurabilityOutcomeUnknown(String),

    #[error("failed to publish scheduler tombstone: {0}")]
    Notification(#[source] crate::notification::NotificationAcknowledgementError),

    #[error("durable scheduler removal requires an acknowledging notification consumer")]
    NoDurableNotificationConsumer,

    #[error("scheduler removal for {0} is pending")]
    RemovalPending(String),

    #[error("scheduler removal cancelled")]
    Cancelled,

    #[error("scheduler removal timed out")]
    Timeout,
}

pub fn scheduler_tool_error(error: SchedulerError) -> xai_tool_runtime::ToolError {
    let code = match &error {
        SchedulerError::InvalidInterval(_)
        | SchedulerError::TaskLimitReached(_)
        | SchedulerError::TaskNotFound(_)
        | SchedulerError::DeliveryUnsupported(_)
        | SchedulerError::InvalidOccurrence(_)
        | SchedulerError::WakeSourceUpdatePending(_) => "scheduler_invalid_request",
        SchedulerError::Persistence(_) => "scheduler_persistence",
        SchedulerError::DurabilityOutcomeUnknown(_) => "scheduler_durability_unknown",
        SchedulerError::Notification(_) => "scheduler_notification",
        SchedulerError::NoDurableNotificationConsumer => "scheduler_durability_unavailable",
        SchedulerError::RemovalPending(_) => "scheduler_removal_pending",
        SchedulerError::Cancelled => "scheduler_cancelled",
        SchedulerError::Timeout => "scheduler_timeout",
    };
    xai_tool_runtime::ToolError::custom(code, error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SchedulerWakeSource {
    Recurrence {
        interval_secs: u64,
        recurring: bool,
    },
    ExternalEvent {
        service: String,
        event: String,
        recurring: bool,
    },
    ProcessSettlement {
        process_id: String,
        command: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingScheduledOccurrence {
    pub id: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveredScheduledOccurrence {
    pub task_id: String,
    pub occurrence_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    pub id: String,
    pub prompt: String,
    pub wake_source: SchedulerWakeSource,
    #[serde(default)]
    pub durable: bool,
    #[serde(default)]
    pub foreground: bool,
    pub created_at: DateTime<Utc>,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_subagent_id: Option<String>,
    #[serde(default)]
    pub iterations_since_fresh: u32,
    /// Set when the prompt is patched: the next fire starts a fresh
    /// transcript instead of resuming the old task's. The anchor itself is
    /// kept until then so the in-flight guard can still see a running
    /// iteration.
    #[serde(default)]
    pub chain_reset_pending: bool,
    #[serde(default)]
    pub pending_occurrences: Vec<PendingScheduledOccurrence>,
    #[serde(default)]
    pub delivered_occurrences: Vec<String>,
}

pub const LOOP_FRESH_CHAIN_EVERY: u32 = 10;

pub const LOOP_COMPLETION_OUTPUT_CAP: usize = 4_000;

/// How long a recurring scheduled task lives before auto-expiry. Single source of truth for the
/// TTL: task construction stamps `expires_at = now + days(this)`, and user-facing copy (pager
/// notice, tool descriptions) must read the same constant so the number cannot drift.
pub const RECURRING_TASK_TTL_DAYS: i64 = 7;

const MAX_SCHEDULER_TRANSITIONS: usize = 50;

impl ScheduledTask {
    pub fn new(interval_secs: u64, prompt: String, recurring: bool, durable: bool) -> Self {
        Self::with_fire_immediately(interval_secs, prompt, recurring, durable, false)
    }

    pub fn with_fire_immediately(
        interval_secs: u64,
        prompt: String,
        recurring: bool,
        durable: bool,
        fire_immediately: bool,
    ) -> Self {
        let now = Utc::now();
        // When fire_immediately is true, anchor created_at in the past so that
        // next_fire_at() = created_at + interval = now, firing on the first tick.
        let created_at = if fire_immediately {
            now - chrono::Duration::seconds(interval_secs as i64)
        } else {
            now
        };
        Self {
            id: uuid::Uuid::now_v7().to_string().replace('-', "")[..12].to_string(),
            prompt,
            wake_source: SchedulerWakeSource::Recurrence {
                interval_secs,
                recurring,
            },
            durable,
            foreground: false,
            created_at,
            last_fired_at: None,
            expires_at: if recurring {
                Some(now + chrono::Duration::days(RECURRING_TASK_TTL_DAYS))
            } else {
                None
            },
            last_subagent_id: None,
            iterations_since_fresh: 0,
            chain_reset_pending: false,
            pending_occurrences: Vec::new(),
            delivered_occurrences: Vec::new(),
        }
    }

    pub fn from_wake_source(
        prompt: String,
        wake_source: SchedulerWakeSource,
        durable: bool,
    ) -> Self {
        let now = Utc::now();
        let recurring = wake_source.is_recurring();
        Self {
            id: uuid::Uuid::now_v7().to_string().replace('-', "")[..12].to_string(),
            prompt,
            wake_source,
            durable,
            foreground: false,
            created_at: now,
            last_fired_at: None,
            expires_at: recurring.then(|| now + chrono::Duration::days(RECURRING_TASK_TTL_DAYS)),
            last_subagent_id: None,
            iterations_since_fresh: 0,
            chain_reset_pending: false,
            pending_occurrences: Vec::new(),
            delivered_occurrences: Vec::new(),
        }
    }

    pub fn is_recurring(&self) -> bool {
        self.wake_source.is_recurring()
    }

    pub fn interval_secs(&self) -> Option<u64> {
        self.wake_source.interval_secs()
    }

    pub fn accepts_delivery(&self) -> bool {
        !matches!(self.wake_source, SchedulerWakeSource::Recurrence { .. })
    }

    pub fn human_schedule(&self) -> String {
        match &self.wake_source {
            SchedulerWakeSource::Recurrence { interval_secs, .. } => {
                super::interval::interval_to_human(*interval_secs)
            }
            SchedulerWakeSource::ExternalEvent { service, event, .. } => {
                format!("{service}: {event}")
            }
            SchedulerWakeSource::ProcessSettlement { command, .. } => {
                format!("when {command} settles")
            }
        }
    }

    /// Next fire time, computed from `last_fired_at` (or `created_at` if never fired).
    pub fn next_fire_at(&self) -> Option<DateTime<Utc>> {
        let anchor = self.last_fired_at.unwrap_or(self.created_at);
        self.interval_secs()
            .map(|seconds| anchor + chrono::Duration::seconds(seconds as i64))
    }

    /// Next moment the actor must wake for this task: the sooner of the next fire and the
    /// auto-expiry deadline. Sleeping purely on `next_fire_at` would let a task whose interval
    /// stretches past `expires_at` outlive the TTL (an 8-day interval must still expire at day
    /// 7, not when its first fire comes due).
    pub fn next_wake_at(&self) -> Option<DateTime<Utc>> {
        match (self.next_fire_at(), self.expires_at) {
            (Some(next), Some(expires_at)) => Some(next.min(expires_at)),
            (Some(next), None) => Some(next),
            (None, expires_at) => expires_at,
        }
    }

    /// Whether this task has expired (recurring tasks only).
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|exp| now >= exp)
    }

    /// The next run still to come; `None` for an expired task or a one-shot that already ran.
    pub fn pending_fire_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.is_expired(now) || (!self.is_recurring() && self.last_fired_at.is_some()) {
            return None;
        }
        self.next_fire_at()
    }
}

impl SchedulerWakeSource {
    pub fn is_recurring(&self) -> bool {
        match self {
            Self::Recurrence { recurring, .. } | Self::ExternalEvent { recurring, .. } => {
                *recurring
            }
            Self::ProcessSettlement { .. } => false,
        }
    }

    pub fn interval_secs(&self) -> Option<u64> {
        match self {
            Self::Recurrence { interval_secs, .. } => Some(*interval_secs),
            Self::ExternalEvent { .. } | Self::ProcessSettlement { .. } => None,
        }
    }
}

/// Persisted state for the scheduler, stored via Resources + ResourcesPersistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchedulerState {
    #[serde(default)]
    pub tasks: Vec<ScheduledTask>,
    /// Durable idempotency receipts for delivered one-shot event/process
    /// tasks, whose task row is removed after firing.
    #[serde(default)]
    pub delivered_occurrences: Vec<DeliveredScheduledOccurrence>,
    #[serde(
        default,
        rename = "occurrenceJournal",
        skip_serializing_if = "super::occurrence_journal::OccurrenceJournal::is_empty"
    )]
    pub(crate) occurrence_journal: super::occurrence_journal::OccurrenceJournal,
}

crate::register_resource!("grok_build", "Scheduler", SchedulerState);

#[derive(Debug, Clone)]
pub struct SchedulerSnapshot {
    // Consumed by the authoritative scheduler snapshot layer in the next migration PR.
    #[allow(dead_code)]
    pub(crate) version: SchedulerVersion,
    pub tasks: Vec<ScheduledTask>,
}

/// Handle for tools to communicate with the SchedulerActor.
/// Ephemeral -- not serialized, not persisted. Inserted via `resources.insert()`.
#[derive(Clone)]
pub struct SchedulerHandle(pub mpsc::UnboundedSender<SchedulerCommand>);

pub enum SchedulerCommand {
    Create {
        task: ScheduledTask,
        reply: oneshot::Sender<Result<ScheduledTask, SchedulerError>>,
    },
    Update {
        id: String,
        prompt: Option<String>,
        wake_source: Option<SchedulerWakeSource>,
        reply: oneshot::Sender<Result<ScheduledTask, SchedulerError>>,
    },
    Deliver {
        id: String,
        occurrence: PendingScheduledOccurrence,
        reply: oneshot::Sender<Result<bool, SchedulerError>>,
    },
    Delete {
        id: String,
        reply: oneshot::Sender<Result<bool, SchedulerError>>,
    },
    List {
        reply: oneshot::Sender<SchedulerSnapshot>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_recurring_task_has_ttl_expiry() {
        let task = ScheduledTask::new(300, "check deploy".into(), true, false);
        assert!(task.expires_at.is_some());
        let expiry = task.expires_at.unwrap();
        let diff = expiry - task.created_at;
        assert_eq!(diff.num_days(), RECURRING_TASK_TTL_DAYS);
    }

    #[test]
    fn new_one_shot_task_has_no_expiry() {
        let task = ScheduledTask::new(300, "check deploy".into(), false, false);
        assert!(task.expires_at.is_none());
    }

    #[test]
    fn next_fire_at_uses_created_at_when_never_fired() {
        let task = ScheduledTask::new(300, "test".into(), true, false);
        let expected = task.created_at + chrono::Duration::seconds(300);
        assert_eq!(task.next_fire_at(), Some(expected));
    }

    #[test]
    fn next_fire_at_uses_last_fired_at_when_present() {
        let mut task = ScheduledTask::new(300, "test".into(), true, false);
        let fired = Utc::now();
        task.last_fired_at = Some(fired);
        let expected = fired + chrono::Duration::seconds(300);
        assert_eq!(task.next_fire_at(), Some(expected));
    }

    #[test]
    fn is_expired_returns_true_when_past_expiry() {
        let mut task = ScheduledTask::new(300, "test".into(), true, false);
        task.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        assert!(task.is_expired(Utc::now()));
    }

    #[test]
    fn is_expired_returns_false_when_before_expiry() {
        let task = ScheduledTask::new(300, "test".into(), true, false);
        assert!(!task.is_expired(Utc::now()));
    }

    #[test]
    fn is_expired_returns_false_for_one_shot() {
        let task = ScheduledTask::new(300, "test".into(), false, false);
        assert!(!task.is_expired(Utc::now()));
    }

    #[test]
    fn host_delivered_sources_have_no_timer_deadline() {
        for source in [
            SchedulerWakeSource::ExternalEvent {
                service: "github".into(),
                event: "pull_request.updated".into(),
                recurring: true,
            },
            SchedulerWakeSource::ProcessSettlement {
                process_id: "process-1".into(),
                command: "cargo test".into(),
            },
        ] {
            let task = ScheduledTask::from_wake_source("inspect".into(), source, false);
            assert!(task.accepts_delivery());
            assert_eq!(task.next_fire_at(), None);
        }
    }

    #[test]
    fn wake_source_wire_is_camel_case_and_round_trips() {
        let source = SchedulerWakeSource::ExternalEvent {
            service: "github".into(),
            event: "pull_request.updated".into(),
            recurring: true,
        };
        let encoded = serde_json::to_value(&source).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "kind": "externalEvent",
                "service": "github",
                "event": "pull_request.updated",
                "recurring": true
            })
        );
        assert_eq!(
            serde_json::from_value::<SchedulerWakeSource>(encoded).unwrap(),
            source
        );
    }

    #[test]
    fn task_id_is_12_chars() {
        let task = ScheduledTask::new(300, "test".into(), true, false);
        assert_eq!(task.id.len(), 12);
    }

    #[test]
    fn clocks_start_with_fresh_uuid_v7_generations() {
        let first = SchedulerClock::new().snapshot();
        let second = SchedulerClock::new().snapshot();

        assert_ne!(first.generation, second.generation);
        assert_eq!(first.generation.get_version_num(), 7);
        assert_eq!(first.revision(), 0);
    }

    #[test]
    fn reservation_preflights_and_commits_in_order() {
        let mut clock = SchedulerClock::new();
        let mut reservation = clock.prepare_transition(2);

        assert_eq!(clock.snapshot().revision(), 0);
        let first = reservation.commit_next(&mut clock);
        assert_eq!(first.version.revision(), 1);
        assert!(first.rollover.is_none());
        assert_eq!(reservation.commit_next(&mut clock).version.revision(), 2);
        assert_eq!(clock.snapshot().revision(), 2);

        let mut boundary = SchedulerClock::at_revision_for_test(u64::MAX - 1);
        let mut final_step = boundary.prepare_transition(1);
        assert_eq!(
            final_step.commit_next(&mut boundary).version.revision(),
            u64::MAX
        );
        let exhausted = SchedulerClock::at_revision_for_test(u64::MAX - 1);
        let old_generation = exhausted.snapshot().generation;
        let reservation = exhausted.prepare_transition(2);
        let rollover = reservation.rollover.unwrap();
        assert_eq!(rollover.old_generation, old_generation);
        assert_ne!(rollover.new_generation, old_generation);
        assert_eq!(exhausted.snapshot().revision(), u64::MAX - 1);
    }

    #[test]
    fn stale_rollover_commit_does_not_mutate_clock() {
        let mut clock = SchedulerClock::at_revision_for_test(u64::MAX);
        let mut stale = clock.prepare_transition(1);
        clock = SchedulerClock::new();
        let before = clock.snapshot();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = stale.commit_next(&mut clock);
        }));

        assert!(result.is_err());
        assert_eq!(clock.snapshot(), before);
    }
}
