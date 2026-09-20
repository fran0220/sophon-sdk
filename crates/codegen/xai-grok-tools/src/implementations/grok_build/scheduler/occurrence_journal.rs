//! Persisted one-shot removal receipts and restart reconciliation.
//!
//! A receipt records task absence and exact fire/removal versions in one JSON resources
//! snapshot. Recovery is a pure plan: it reports removals requiring persistence and
//! timer suppression while all state mutation/publication remains in the actor layer.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use super::types::{ScheduledTask, SchedulerState, SchedulerVersion};

pub(super) const MAX_PENDING_ONE_SHOTS: usize = 50;
const MAX_QUARANTINED_TASK_IDS: usize = 50;
const MAX_TASK_ID_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct ScheduledOccurrenceId(uuid::Uuid);

impl ScheduledOccurrenceId {
    fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

impl<'de> Deserialize<'de> for ScheduledOccurrenceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let id = uuid::Uuid::deserialize(deserializer)?;
        if id.get_version() != Some(uuid::Version::SortRand)
            || id.get_variant() != uuid::Variant::RFC4122
        {
            return Err(serde::de::Error::custom(
                "scheduled occurrence identity must be an RFC UUIDv7",
            ));
        }
        Ok(Self(id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduledOccurrenceVersions {
    fire: SchedulerVersion,
    removal: SchedulerVersion,
}

impl ScheduledOccurrenceVersions {
    pub(super) fn try_new(
        fire: SchedulerVersion,
        removal: SchedulerVersion,
    ) -> Result<Self, OccurrenceJournalError> {
        let generation = fire.generation_id();
        if generation.get_version() != Some(uuid::Version::SortRand)
            || generation.get_variant() != uuid::Variant::RFC4122
            || fire.revision() == 0
            || removal.generation_id() != generation
            || fire
                .revision()
                .checked_add(1)
                .is_none_or(|revision| removal.revision() != revision)
        {
            return Err(OccurrenceJournalError::InvalidVersions);
        }
        Ok(Self { fire, removal })
    }

    pub(super) fn fire(self) -> SchedulerVersion {
        self.fire
    }

    pub(super) fn removal(self) -> SchedulerVersion {
        self.removal
    }

    fn contains(self, version: SchedulerVersion) -> bool {
        self.fire == version || self.removal == version
    }
}

impl<'de> Deserialize<'de> for ScheduledOccurrenceVersions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PersistedVersions {
            fire: SchedulerVersion,
            removal: SchedulerVersion,
        }

        let persisted = PersistedVersions::deserialize(deserializer)?;
        Self::try_new(persisted.fire, persisted.removal).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OneShotOccurrence {
    occurrence_id: ScheduledOccurrenceId,
    task: ScheduledTask,
    versions: ScheduledOccurrenceVersions,
}

impl<'de> Deserialize<'de> for OneShotOccurrence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PersistedOccurrence {
            occurrence_id: ScheduledOccurrenceId,
            task: ScheduledTask,
            versions: ScheduledOccurrenceVersions,
        }

        let persisted = PersistedOccurrence::deserialize(deserializer)?;
        if persisted.task.recurring || !persisted.task.durable {
            return Err(serde::de::Error::custom(
                OccurrenceJournalError::NotDurableOneShot(persisted.task.id),
            ));
        }
        Ok(Self {
            occurrence_id: persisted.occurrence_id,
            task: persisted.task,
            versions: persisted.versions,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OccurrenceJournal {
    entries: Vec<OneShotOccurrence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    quarantined_task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    block_all_one_shots: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    overflowed: bool,
}

impl OccurrenceJournal {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.quarantined_task_ids.is_empty()
            && !self.block_all_one_shots
            && !self.overflowed
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn quarantine_diagnostics(&self) -> (&[String], bool, bool) {
        (
            &self.quarantined_task_ids,
            self.block_all_one_shots,
            self.overflowed,
        )
    }
}

/// Current persisted schema only; malformed receipts are never dropped or normalized.
fn deserialize_current<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<OccurrenceJournal, D::Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Current {
        entries: Vec<OneShotOccurrence>,
        #[serde(default)]
        quarantined_task_ids: Vec<String>,
        #[serde(default)]
        block_all_one_shots: bool,
        #[serde(default)]
        overflowed: bool,
    }
    let value = serde_json::Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(serde::de::Error::custom(
            "scheduler occurrence journal must be a current-schema object",
        ));
    }
    let current: Current = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
    if current.entries.len() > MAX_PENDING_ONE_SHOTS
        || current.quarantined_task_ids.len() > MAX_QUARANTINED_TASK_IDS
        || (current.overflowed && !current.block_all_one_shots)
        || current
            .quarantined_task_ids
            .iter()
            .enumerate()
            .any(|(index, id)| {
                id.is_empty()
                    || id.len() > MAX_TASK_ID_BYTES
                    || current.quarantined_task_ids[..index].contains(id)
            })
    {
        return Err(serde::de::Error::custom(
            "unsupported or malformed scheduler occurrence journal",
        ));
    }
    for occurrence in &current.entries {
        occurrence
            .task
            .validate()
            .map_err(serde::de::Error::custom)?;
    }
    Ok(OccurrenceJournal {
        entries: current.entries,
        quarantined_task_ids: current.quarantined_task_ids,
        block_all_one_shots: current.block_all_one_shots,
        overflowed: current.overflowed,
    })
}

impl<'de> Deserialize<'de> for OccurrenceJournal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_current(deserializer)
    }
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub(crate) enum OccurrenceJournalError {
    #[error("scheduled task {0} was not found")]
    TaskNotFound(String),

    #[error("scheduled task {0} is not a durable one-shot")]
    NotDurableOneShot(String),

    #[error("scheduled task {0} already has a pending occurrence")]
    TaskAlreadyJournaled(String),

    #[error("maximum of {MAX_PENDING_ONE_SHOTS} pending one-shot occurrences reached")]
    JournalFull,

    #[error("one-shot fire/removal versions must be nonzero consecutive RFC UUIDv7 transitions")]
    InvalidVersions,

    #[error("scheduler transition version is already journaled")]
    DuplicateTransitionVersion,

    #[error("one-shot journal requires manual recovery before new occurrences can be prepared")]
    RecoveryRequired,

    #[error("scheduled occurrence was not found")]
    OccurrenceNotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OneShotJournalConflict {
    OccurrenceId,
    TaskId,
    TransitionVersion,
}

#[must_use = "loaded one-shot receipts must suppress timers and reconcile resources"]
pub(crate) struct SchedulerLoadReconciliation {
    requires_resources_persistence: bool,
    task_ids_to_remove: Vec<String>,
    blocked_task_ids: HashSet<String>,
    block_all_one_shots: bool,
    recovery_required: bool,
    conflicts: Vec<OneShotJournalConflict>,
    overflow_error: Option<OccurrenceJournalError>,
}

impl SchedulerLoadReconciliation {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn requires_resources_persistence(&self) -> bool {
        self.requires_resources_persistence
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn task_ids_to_remove(&self) -> &[String] {
        &self.task_ids_to_remove
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn blocked_task_ids(&self) -> &HashSet<String> {
        &self.blocked_task_ids
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn block_all_one_shots(&self) -> bool {
        self.block_all_one_shots
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn recovery_required(&self) -> bool {
        self.recovery_required
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn conflicts(&self) -> &[OneShotJournalConflict] {
        &self.conflicts
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn overflow_error(&self) -> Option<&OccurrenceJournalError> {
        self.overflow_error.as_ref()
    }
}

impl SchedulerState {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn prepare_one_shot_occurrence(
        &mut self,
        task_id: &str,
        versions: ScheduledOccurrenceVersions,
    ) -> Result<OneShotOccurrence, OccurrenceJournalError> {
        self.prepare_one_shot_occurrence_with_id(ScheduledOccurrenceId::new(), task_id, versions)
    }

    fn prepare_one_shot_occurrence_with_id(
        &mut self,
        occurrence_id: ScheduledOccurrenceId,
        task_id: &str,
        versions: ScheduledOccurrenceVersions,
    ) -> Result<OneShotOccurrence, OccurrenceJournalError> {
        if !self.occurrence_journal.quarantined_task_ids.is_empty()
            || self.occurrence_journal.block_all_one_shots
            || self.occurrence_journal.overflowed
            || has_conflict(&self.occurrence_journal.entries)
        {
            return Err(OccurrenceJournalError::RecoveryRequired);
        }
        if self.occurrence_journal.entries.len() >= MAX_PENDING_ONE_SHOTS {
            return Err(OccurrenceJournalError::JournalFull);
        }
        if self
            .occurrence_journal
            .entries
            .iter()
            .any(|occurrence| occurrence.task.id == task_id)
        {
            return Err(OccurrenceJournalError::TaskAlreadyJournaled(
                task_id.to_owned(),
            ));
        }
        if self.occurrence_journal.entries.iter().any(|occurrence| {
            occurrence.versions.contains(versions.fire())
                || occurrence.versions.contains(versions.removal())
        }) {
            return Err(OccurrenceJournalError::DuplicateTransitionVersion);
        }
        let index = self
            .tasks
            .iter()
            .position(|task| task.id == task_id)
            .ok_or_else(|| OccurrenceJournalError::TaskNotFound(task_id.to_owned()))?;
        let Some(task) = self.tasks.get(index) else {
            return Err(OccurrenceJournalError::TaskNotFound(task_id.to_owned()));
        };
        if task.recurring || !task.durable {
            return Err(OccurrenceJournalError::NotDurableOneShot(
                task_id.to_owned(),
            ));
        }

        let occurrence = OneShotOccurrence {
            occurrence_id,
            task: self.tasks.remove(index),
            versions,
        };
        self.occurrence_journal.entries.push(occurrence.clone());
        Ok(occurrence)
    }

    #[must_use = "the exact removal receipt must be durably cleared"]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn finish_one_shot_removal(
        &mut self,
        occurrence_id: &ScheduledOccurrenceId,
    ) -> Result<OneShotOccurrence, OccurrenceJournalError> {
        let index = self
            .occurrence_journal
            .entries
            .iter()
            .position(|occurrence| occurrence.occurrence_id == *occurrence_id)
            .ok_or(OccurrenceJournalError::OccurrenceNotFound)?;
        Ok(self.occurrence_journal.entries.remove(index))
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "wired by durable one-shot actor layer")
    )]
    pub(super) fn reconcile_one_shot_occurrences(&self) -> SchedulerLoadReconciliation {
        let occurrence_counts = count_by(self.occurrence_journal.entries.iter(), |entry| {
            entry.occurrence_id.clone()
        });
        let task_counts = count_by(self.occurrence_journal.entries.iter(), |entry| {
            entry.task.id.clone()
        });
        let mut version_counts = HashMap::new();
        for occurrence in &self.occurrence_journal.entries {
            for version in [occurrence.versions.fire(), occurrence.versions.removal()] {
                *version_counts.entry(version).or_insert(0usize) += 1;
            }
        }
        let conflict_for = |occurrence: &OneShotOccurrence| {
            if occurrence_counts
                .get(&occurrence.occurrence_id)
                .copied()
                .is_some_and(|count| count > 1)
            {
                Some(OneShotJournalConflict::OccurrenceId)
            } else if task_counts
                .get(&occurrence.task.id)
                .copied()
                .is_some_and(|count| count > 1)
            {
                Some(OneShotJournalConflict::TaskId)
            } else if version_counts
                .get(&occurrence.versions.fire())
                .copied()
                .is_some_and(|count| count > 1)
                || version_counts
                    .get(&occurrence.versions.removal())
                    .copied()
                    .is_some_and(|count| count > 1)
            {
                Some(OneShotJournalConflict::TransitionVersion)
            } else {
                None
            }
        };

        let mut blocked_task_ids: HashSet<String> = self
            .occurrence_journal
            .quarantined_task_ids
            .iter()
            .cloned()
            .collect();
        let block_all_one_shots = self.occurrence_journal.block_all_one_shots;
        let overflowed = self.occurrence_journal.overflowed;
        let conflicts: Vec<_> = self
            .occurrence_journal
            .entries
            .iter()
            .filter_map(conflict_for)
            .collect();
        let recovery_required = block_all_one_shots
            || overflowed
            || !self.occurrence_journal.quarantined_task_ids.is_empty()
            || !conflicts.is_empty();
        blocked_task_ids.extend(
            self.occurrence_journal
                .entries
                .iter()
                .map(|occurrence| occurrence.task.id.clone()),
        );
        if block_all_one_shots {
            blocked_task_ids.extend(
                self.tasks
                    .iter()
                    .filter(|task| !task.recurring)
                    .map(|task| task.id.clone()),
            );
        }

        let task_ids_to_remove: Vec<String> = if recovery_required {
            Vec::new()
        } else {
            let journaled: HashSet<&str> = self
                .occurrence_journal
                .entries
                .iter()
                .map(|occurrence| occurrence.task.id.as_str())
                .collect();
            self.tasks
                .iter()
                .filter(|task| journaled.contains(task.id.as_str()))
                .map(|task| task.id.clone())
                .collect()
        };

        SchedulerLoadReconciliation {
            requires_resources_persistence: !task_ids_to_remove.is_empty(),
            task_ids_to_remove,
            blocked_task_ids,
            block_all_one_shots,
            recovery_required,
            conflicts,
            overflow_error: overflowed.then_some(OccurrenceJournalError::JournalFull),
        }
    }
}

fn has_conflict(entries: &[OneShotOccurrence]) -> bool {
    let occurrence_ids: HashSet<_> = entries.iter().map(|entry| &entry.occurrence_id).collect();
    let task_ids: HashSet<_> = entries.iter().map(|entry| entry.task.id.as_str()).collect();
    let versions: HashSet<_> = entries
        .iter()
        .flat_map(|entry| [entry.versions.fire(), entry.versions.removal()])
        .collect();
    occurrence_ids.len() != entries.len()
        || task_ids.len() != entries.len()
        || versions.len() != entries.len() * 2
}

fn count_by<'a, T, K>(
    values: impl Iterator<Item = &'a T>,
    key: impl Fn(&T) -> K,
) -> HashMap<K, usize>
where
    T: 'a,
    K: Eq + std::hash::Hash,
{
    let mut counts = HashMap::new();
    for value in values {
        *counts.entry(key(value)).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
#[path = "occurrence_journal_tests.rs"]
mod tests;
