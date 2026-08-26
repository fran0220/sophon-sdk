use super::super::*;

#[test]
fn pending_rewind_never_guesses_from_prompt_count_when_prefix_identity_drifted() {
    let ledger = SessionLedger {
        entries: vec![SessionLedgerEntry {
            turn_id: "turn-0".into(),
            prompt_digest: "sha256:expected".into(),
            runtime_prompt_index: 0,
            state: LedgerTurnState::Completed {
                outcome: TurnOutcome::End,
                settlement_id: "settlement-0".into(),
                usage: None,
            },
            source: InputSource::User,
        }],
    };
    let drifted = RewindPointWire {
        prompt_index: 0,
        created_at: "2026-08-07T00:00:00Z".into(),
        num_file_snapshots: 0,
        has_file_changes: false,
        prompt_preview: None,
        origin_prompt_index: None,
        origin_prompt_digest: Some("sha256:other".into()),
    };

    assert!(native_rewind_target(&[drifted], 1, "sha256:missing", &ledger, true).is_err());
}

#[test]
fn native_rewind_identity_maps_residency_index_to_durable_ledger_index() {
    let ledger = SessionLedger {
        entries: (0..4)
            .map(|index| SessionLedgerEntry {
                turn_id: format!("turn-{index}"),
                prompt_digest: format!("sha256:prompt-{index}"),
                runtime_prompt_index: index,
                state: LedgerTurnState::Completed {
                    outcome: TurnOutcome::End,
                    settlement_id: format!("settlement-{index}"),
                    usage: None,
                },
                source: InputSource::User,
            })
            .collect(),
    };
    let resumed_point = RewindPointWire {
        prompt_index: 0,
        created_at: "2026-08-26T00:00:00Z".into(),
        num_file_snapshots: 0,
        has_file_changes: false,
        prompt_preview: Some("resumed prompt".into()),
        origin_prompt_index: Some(3),
        origin_prompt_digest: Some("sha256:prompt-3".into()),
    };

    assert_eq!(
        map_native_rewind_points(&[resumed_point], &ledger).unwrap(),
        vec![3]
    );
    assert_eq!(
        native_rewind_target(
            &[RewindPointWire {
                prompt_index: 0,
                created_at: String::new(),
                num_file_snapshots: 0,
                has_file_changes: false,
                prompt_preview: None,
                origin_prompt_index: Some(3),
                origin_prompt_digest: Some("sha256:prompt-3".into()),
            }],
            3,
            "sha256:prompt-3",
            &ledger,
            false,
        )
        .unwrap(),
        Some(0)
    );
}
