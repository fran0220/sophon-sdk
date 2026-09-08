use super::*;

#[test]
fn consumed_index_is_prompt_bound_and_reset_on_promotion() {
    let slot = TurnReportSlot::default();
    assert_eq!(slot.prompt_index("first"), None);
    slot.consume_index("first", 7);
    assert_eq!(slot.prompt_index("first"), Some(7));
    assert_eq!(slot.prompt_index("queued"), None);
    slot.start_next_turn();
    assert_eq!(slot.prompt_index("first"), None);
    assert_eq!(slot.prompt_index("queued"), None);
    slot.consume_index("queued", 8);
    assert_eq!(slot.prompt_index("queued"), Some(8));
}

#[test]
fn a_release_for_a_stale_epoch_leaves_the_live_claim_alone() {
    let slot = TurnReportSlot::default();
    let stale = slot.epoch();
    slot.start_next_turn();

    let live = slot.claim_for_gate().expect("the successor claims");
    slot.release_aborted(stale);

    assert!(matches!(slot.state(), TurnReportState::Held { .. }));
    assert_eq!(live.commit(), CommitOutcome::Reported);
}

#[test]
fn a_release_does_not_reopen_a_reported_slot() {
    let slot = TurnReportSlot::default();
    let claim = slot.claim_for_gate().expect("the gate claims");
    assert_eq!(claim.commit(), CommitOutcome::Reported);

    slot.release_aborted(slot.epoch());

    assert_eq!(slot.state(), TurnReportState::Reported);
}

#[test]
fn a_released_claim_cannot_commit_over_its_successor() {
    let slot = TurnReportSlot::default();
    let epoch = slot.epoch();

    let first = slot.claim_for_gate().expect("the slot is free");
    slot.release_aborted(epoch);
    let second = slot.claim_for_gate().expect("the release freed it");

    assert_eq!(first.commit(), CommitOutcome::LostToAnotherReporter);
    assert!(matches!(slot.state(), TurnReportState::Held { .. }));
    assert_eq!(second.commit(), CommitOutcome::Reported);
}

#[test]
fn an_abort_cannot_release_a_report_claim() {
    let slot = TurnReportSlot::default();
    let epoch = slot.epoch();

    let report = slot.claim_at(epoch).expect("the report path claims");
    slot.release_aborted(epoch);

    assert!(matches!(slot.state(), TurnReportState::Held { .. }));
    assert!(
        slot.claim_at(epoch).is_none(),
        "the release must not have handed the turn to a second reporter"
    );
    assert_eq!(report.commit(), CommitOutcome::Reported);
}
