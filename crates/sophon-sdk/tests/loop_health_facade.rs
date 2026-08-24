use sophon_sdk::{LoopHealthLimitReason, TurnOutcome, run};

#[test]
fn loop_health_limit_is_a_typed_turn_and_effect_contract() {
    let outcome = TurnOutcome::BudgetLimited {
        reason: LoopHealthLimitReason::Repetition { repeated_steps: 16 },
    };
    let encoded = serde_json::to_value(outcome).expect("Turn outcome serializes");
    assert_eq!(
        encoded["BudgetLimited"]["reason"],
        serde_json::json!({"kind": "repetition", "repeatedSteps": 16})
    );
    assert_eq!(
        serde_json::from_value::<TurnOutcome>(encoded).expect("Turn outcome round-trips"),
        outcome
    );

    let session = run::SessionRef::new("session-loop-health").expect("session ref");
    let usage = run::EffectUsage::default();
    let receipt = run::EffectReceipt::for_session_turn(
        &session,
        "turn-1",
        "sha256:prompt",
        0,
        run::SessionTurnOutcome::BudgetLimited,
        usage.clone(),
        usage,
    );
    assert_eq!(
        receipt.outcome,
        Some(run::SessionTurnOutcome::BudgetLimited)
    );
    assert!(
        receipt
            .settlement_id
            .as_deref()
            .unwrap()
            .starts_with("sha256:")
    );
}
