use sophon_sdk::{EventUpdate, InteractionKind, InteractionResolution};

#[test]
fn structured_elicitation_exposes_pending_answered_and_truthful_unanswered_states() {
    let opened = EventUpdate::InteractionOpened {
        id: "ask-1".into(),
        kind: InteractionKind::Question,
    };
    let answered = EventUpdate::InteractionResolved {
        id: "ask-1".into(),
        resolution: InteractionResolution::Answered,
    };
    let unanswered = EventUpdate::InteractionResolved {
        id: "ask-2".into(),
        resolution: InteractionResolution::Unanswered,
    };

    for update in [opened, answered, unanswered] {
        let encoded = serde_json::to_value(&update).expect("interaction event serializes");
        assert_eq!(
            serde_json::from_value::<EventUpdate>(encoded).expect("interaction event round-trips"),
            update
        );
    }
}
