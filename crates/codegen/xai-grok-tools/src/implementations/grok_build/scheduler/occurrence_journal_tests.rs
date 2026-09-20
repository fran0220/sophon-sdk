use super::*;
use crate::persistence::ResourcesPersistence;
use crate::types::resources::{Resources, State};
use chrono::{TimeZone, Utc};

const GENERATION: &str = "01890f42-7d5c-7c00-8000-000000000001";

fn uuid(suffix: u64) -> uuid::Uuid {
    uuid::Uuid::parse_str(&format!("01890f42-7d5c-7c00-8000-{suffix:012x}")).unwrap()
}

fn task(id: &str, recurring: bool, durable: bool) -> ScheduledTask {
    ScheduledTask {
        id: id.into(),
        cadence: if recurring {
            super::super::types::SchedulerCadence::Interval {
                every_secs: 300,
                anchor: Utc.timestamp_opt(1_700_000_300, 0).unwrap(),
            }
        } else {
            super::super::types::SchedulerCadence::Once {
                at: Utc.timestamp_opt(1_700_000_300, 0).unwrap(),
            }
        },
        next_run_at: Some(Utc.timestamp_opt(1_700_000_300, 0).unwrap()),
        last_dispatch: None,
        interval_secs: 300,
        prompt: format!("run {id}"),
        recurring,
        durable,
        created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        last_fired_at: None,
        expires_at: None,
        last_subagent_id: None,
        iterations_since_fresh: 0,
        chain_reset_pending: false,
    }
}

fn version(generation: &str, revision: u64) -> SchedulerVersion {
    SchedulerVersion::from_parts(uuid::Uuid::parse_str(generation).unwrap(), revision)
}

fn versions(revision: u64) -> ScheduledOccurrenceVersions {
    ScheduledOccurrenceVersions::try_new(
        version(GENERATION, revision),
        version(GENERATION, revision + 1),
    )
    .unwrap()
}

fn occurrence_json(
    id: &str,
    task: serde_json::Value,
    versions: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({ "occurrenceId": id, "task": task, "versions": versions })
}

fn valid_occurrence_json(id_suffix: u64, task_id: &str, revision: u64) -> serde_json::Value {
    occurrence_json(
        &uuid(id_suffix).to_string(),
        serde_json::to_value(task(task_id, false, true)).unwrap(),
        serde_json::json!({
            "fire": { "generation": GENERATION, "revision": revision },
            "removal": { "generation": GENERATION, "revision": revision + 1 },
        }),
    )
}

fn state(tasks: Vec<ScheduledTask>, journal: serde_json::Value) -> SchedulerState {
    serde_json::from_value(serde_json::json!({
        "tasks": tasks,
        "occurrenceJournal": journal
    }))
    .unwrap()
}

fn prepare(state: &mut SchedulerState, task_id: &str, revision: u64) -> OneShotOccurrence {
    state
        .prepare_one_shot_occurrence_with_id(
            ScheduledOccurrenceId(uuid(100 + revision)),
            task_id,
            versions(revision),
        )
        .unwrap()
}

#[test]
fn prepare_finish_and_mutation_failures_preserve_state() {
    let mut state = SchedulerState {
        tasks: vec![task("one-shot", false, true), task("second", false, true)],
        ..Default::default()
    };
    let occurrence = prepare(&mut state, "one-shot", 7);
    assert_eq!(occurrence.task.id, "one-shot");
    state
        .finish_one_shot_removal(&occurrence.occurrence_id)
        .unwrap();

    prepare(&mut state, "second", 1);
    state.tasks.push(task("duplicate", false, true));
    assert_eq!(
        state
            .prepare_one_shot_occurrence("duplicate", versions(1))
            .unwrap_err(),
        OccurrenceJournalError::DuplicateTransitionVersion
    );

    for invalid in [
        task("recurring", true, true),
        task("ephemeral", false, false),
    ] {
        let mut state = SchedulerState {
            tasks: vec![invalid.clone()],
            ..Default::default()
        };
        assert!(matches!(
            state.prepare_one_shot_occurrence(&invalid.id, versions(3)),
            Err(OccurrenceJournalError::NotDurableOneShot(_))
        ));
    }
}

#[test]
fn validation_rejects_impossible_versions_and_non_rfc_identity() {
    for (fire_generation, removal_generation, fire, removal) in [
        (GENERATION, GENERATION, 0, 1),
        (GENERATION, GENERATION, 1, 3),
        (GENERATION, "01890f42-7d5c-7c00-8000-000000000002", 1, 2),
        ("01890f42-7d5c-7c00-c000-000000000001", GENERATION, 1, 2),
    ] {
        assert_eq!(
            ScheduledOccurrenceVersions::try_new(
                version(fire_generation, fire),
                version(removal_generation, removal),
            ),
            Err(OccurrenceJournalError::InvalidVersions)
        );
    }

    let invalid = occurrence_json(
        "01890f42-7d5c-7c00-c000-000000000001",
        serde_json::to_value(task("bad-id", false, true)).unwrap(),
        serde_json::json!({
            "fire": { "generation": GENERATION, "revision": 1 },
            "removal": { "generation": GENERATION, "revision": 2 },
        }),
    );
    assert!(
        serde_json::from_value::<OccurrenceJournal>(serde_json::json!({"entries": [invalid]}))
            .is_err()
    );
}

#[test]
fn exactly_fifty_round_trips_and_mutation_reports_journal_full() {
    let entries: Vec<_> = (0..MAX_PENDING_ONE_SHOTS)
        .map(|index| {
            valid_occurrence_json(
                100 + index as u64,
                &format!("task-{index}"),
                index as u64 * 2 + 1,
            )
        })
        .collect();
    let mut state = state(Vec::new(), serde_json::json!({"entries": entries}));
    assert_eq!(
        state.occurrence_journal.entries.len(),
        MAX_PENDING_ONE_SHOTS
    );
    let encoded = serde_json::to_value(&state).unwrap();
    let reloaded: SchedulerState = serde_json::from_value(encoded).unwrap();
    assert_eq!(
        reloaded.occurrence_journal.entries.len(),
        MAX_PENDING_ONE_SHOTS
    );

    state.tasks.push(task("new", false, true));
    assert_eq!(
        state
            .prepare_one_shot_occurrence("new", versions(3))
            .unwrap_err(),
        OccurrenceJournalError::JournalFull
    );
}

#[test]
fn overflow_is_rejected_without_dropping_tail() {
    let mut entries: Vec<_> = (0..MAX_PENDING_ONE_SHOTS)
        .map(|index| valid_occurrence_json(200 + index as u64, &format!("task-{index}"), 1))
        .collect();
    entries.push(valid_occurrence_json(999, "tail-task", 3));
    assert!(
        serde_json::from_value::<OccurrenceJournal>(serde_json::json!({"entries": entries}))
            .is_err()
    );
}

#[test]
fn malformed_receipts_are_rejected_without_normalization() {
    let malformed = occurrence_json(
        &uuid(20).to_string(),
        serde_json::json!({ "prompt": "missing id" }),
        serde_json::json!({
            "fire": { "generation": GENERATION, "revision": 1 },
            "removal": { "generation": GENERATION, "revision": 2 },
        }),
    );
    assert!(
        serde_json::from_value::<OccurrenceJournal>(serde_json::json!({"entries": [malformed]}))
            .is_err()
    );
}

#[test]
fn inconsistent_current_metadata_and_legacy_arrays_are_rejected() {
    let current = serde_json::json!({
        "entries": [],
        "overflowed": true,
        "blockAllOneShots": false,
    });
    assert!(serde_json::from_value::<OccurrenceJournal>(current).is_err());
    assert!(serde_json::from_value::<OccurrenceJournal>(serde_json::json!([])).is_err());
    assert!(
        serde_json::from_value::<OccurrenceJournal>(serde_json::json!([valid_occurrence_json(
            10, "old", 1
        )]))
        .is_err()
    );
}

#[test]
fn current_recovery_metadata_roundtrips_without_normalization() {
    let current = serde_json::json!({
        "entries": [],
        "quarantinedTaskIds": ["held"],
        "blockAllOneShots": true,
        "overflowed": true,
    });
    let loaded = state(vec![task("held", false, true)], current.clone());
    assert_eq!(
        serde_json::to_value(&loaded.occurrence_journal).unwrap(),
        current
    );
    let (ids, blocked, overflowed) = loaded.occurrence_journal.quarantine_diagnostics();
    assert_eq!(ids, ["held"]);
    assert!(blocked && overflowed);
    let plan = loaded.reconcile_one_shot_occurrences();
    assert!(plan.block_all_one_shots() && plan.recovery_required());
    assert!(plan.overflow_error().is_some());
    assert!(plan.blocked_task_ids().contains("held"));
}

#[tokio::test]
async fn production_loader_rejects_old_or_malformed_journal_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resources_state.json");
    let invalid = occurrence_json(
        &uuid(30).to_string(),
        serde_json::to_value(task("bad", true, true)).unwrap(),
        serde_json::json!({
            "fire": { "generation": GENERATION, "revision": 1 },
            "removal": { "generation": GENERATION, "revision": 2 },
        }),
    );
    for journal in [
        serde_json::json!([]),
        serde_json::json!([valid_occurrence_json(10, "old", 1)]),
        serde_json::json!({"entries": [invalid]}),
        serde_json::json!({ "entries": "bad", "blockAllOneShots": [] }),
        serde_json::json!({ "quarantinedTaskIds": ["kept-id", 7] }),
        serde_json::json!("wrong-shape"),
    ] {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "state": { "grok_build.Scheduler": {
                "tasks": [task("kept", true, true)],
                "occurrenceJournal": journal
            } }
        }))
        .unwrap();
        std::fs::write(&path, &bytes).unwrap();
        for _ in 0..2 {
            let mut resources = Resources::new();
            resources.register_state::<SchedulerState>();
            let persistence = ResourcesPersistence::new(path.clone());
            assert_eq!(
                persistence.load(&mut resources).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
            assert!(resources.get::<State<SchedulerState>>().is_none());
            persistence.flush().await;
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}

#[test]
fn reconciliation_exposes_only_persistence_and_suppression_foundation() {
    let state = state(
        vec![task("resurrected", false, true)],
        serde_json::json!({"entries": [valid_occurrence_json(10, "resurrected", 1)]}),
    );
    let plan = state.reconcile_one_shot_occurrences();
    assert!(plan.requires_resources_persistence());
    assert_eq!(plan.task_ids_to_remove(), ["resurrected"]);
    let Some(first) = state.tasks.first() else {
        panic!("expected a loaded task: {:?}", state.tasks);
    };
    assert_eq!(first.id, "resurrected");
}

#[test]
fn conflict_receipts_produce_diagnostics_and_suppress_every_task() {
    for (entries, expected) in [
        (
            vec![
                valid_occurrence_json(10, "first", 1),
                valid_occurrence_json(10, "second", 3),
            ],
            OneShotJournalConflict::OccurrenceId,
        ),
        (
            vec![
                valid_occurrence_json(10, "same", 1),
                valid_occurrence_json(11, "same", 3),
            ],
            OneShotJournalConflict::TaskId,
        ),
        (
            vec![
                valid_occurrence_json(10, "first", 1),
                valid_occurrence_json(11, "second", 1),
            ],
            OneShotJournalConflict::TransitionVersion,
        ),
    ] {
        let ids: Vec<_> = entries
            .iter()
            .map(|entry| {
                let Some(id) = entry
                    .get("task")
                    .and_then(|t| t.get("id"))
                    .and_then(|v| v.as_str())
                else {
                    panic!("entry missing task.id: {entry}");
                };
                id.to_owned()
            })
            .collect();
        let mut state = state(
            ids.iter().map(|id| task(id, false, true)).collect(),
            serde_json::json!({"entries": entries}),
        );
        let plan = state.reconcile_one_shot_occurrences();
        assert!(plan.recovery_required());
        assert!(plan.task_ids_to_remove().is_empty());
        assert_eq!(plan.conflicts(), &[expected, expected]);
        assert!(ids.iter().all(|id| plan.blocked_task_ids().contains(id)));
        assert_eq!(state.tasks.len(), ids.len());
        let unrelated = "unrelated";
        state.tasks.push(task(unrelated, false, true));
        let before = state.tasks.len();
        assert_eq!(
            state
                .prepare_one_shot_occurrence(unrelated, versions(9))
                .unwrap_err(),
            OccurrenceJournalError::RecoveryRequired
        );
        assert_eq!(state.tasks.len(), before);
    }
}

#[test]
fn empty_journal_omits_legacy_field() {
    let serialized = serde_json::to_value(SchedulerState {
        tasks: vec![task("legacy", true, true)],
        ..Default::default()
    })
    .unwrap();
    assert!(serialized.get("occurrenceJournal").is_none());
}
