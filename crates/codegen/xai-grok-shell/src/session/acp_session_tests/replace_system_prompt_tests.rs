//! Actor-path coverage for `handle_replace_system_prompt`, the resident-reconnect `systemPromptOverride` sync.
//! The head swap itself is unit-tested in `xai_chat_state` (`conversation_util` and the actor tests).
//! These tests cover only what is unique to the `SessionActor` path: the end-to-end swap and the `preserve_inherited_system` skip.

use xai_grok_sampling_types::conversation::ConversationItem;

use super::support::create_test_actor;
use super::{PersistenceMsg, SessionActor};

fn head_text(conv: &[ConversationItem]) -> Option<String> {
    match conv.first() {
        Some(ConversationItem::System(sys)) => Some(sys.content.to_string()),
        _ => None,
    }
}

async fn actor_with_history(history: Vec<ConversationItem>) -> SessionActor {
    let (gateway_tx, _grx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.chat_state_handle.replace_conversation(history);
    actor
}

/// Exercise the actual model-update and override actor paths with a persisted
/// authored head, not the stock agent template. No model/network turn is needed.
#[tokio::test(flavor = "current_thread")]
async fn attach_restore_retains_original_rules_and_explicit_override_wins() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for original in [
                "Original system prompt\nOriginal project rules",
                "ADMISSION explicit persisted override",
            ] {
                let actor = std::sync::Arc::new(
                    actor_with_history(vec![
                        ConversationItem::system(original),
                        ConversationItem::user("existing turn"),
                        ConversationItem::assistant("existing reply"),
                    ])
                    .await,
                );
                let cfg = xai_grok_sampler::SamplerConfig {
                    model: "routing-slug".to_owned(),
                    context_window: 256_000,
                    ..Default::default()
                };
                let model = actor
                    .handle_set_session_model(cfg, false, false, true, true, 75)
                    .await
                    .unwrap();
                assert_eq!(model.0.as_ref(), "routing-slug");
                assert_eq!(actor.compaction.threshold_percent.get(), 75);
                let restored = actor.chat_state_handle.get_conversation().await;
                assert_eq!(head_text(&restored).as_deref(), Some(original));
                assert_eq!(restored.len(), 3);

                actor
                    .handle_replace_system_prompt("ADMISSION replacement".to_owned())
                    .await;
                let replaced = actor.chat_state_handle.get_conversation().await;
                assert_eq!(
                    head_text(&replaced).as_deref(),
                    Some("ADMISSION replacement")
                );
                assert_eq!(replaced.len(), 3);
                assert_eq!(
                    serde_json::to_value(&replaced[1..]).unwrap(),
                    serde_json::to_value(&restored[1..]).unwrap()
                );
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn handle_replace_system_prompt_replaces_head_and_preserves_turns() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_history(vec![
                ConversationItem::system("composer default"),
                ConversationItem::user("hi"),
                ConversationItem::assistant("yo"),
            ])
            .await;

            actor
                .handle_replace_system_prompt("client override".to_string())
                .await;

            let conv = actor.chat_state_handle.get_conversation().await;
            assert_eq!(head_text(&conv).as_deref(), Some("client override"));
            assert_eq!(conv.len(), 3, "must not wipe user/assistant turns");
            assert!(matches!(conv[1], ConversationItem::User(_)));
            assert!(matches!(conv[2], ConversationItem::Assistant(_)));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn handle_replace_system_prompt_skips_on_preserve_inherited_system() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = actor_with_history(vec![
                ConversationItem::system("parent verbatim"),
                ConversationItem::user("hi"),
            ])
            .await;
            // Verbatim mirror-fork: the inherited cache prefix must survive.
            actor.startup_hints.preserve_inherited_system = true;

            actor
                .handle_replace_system_prompt("client override".to_string())
                .await;

            let conv = actor.chat_state_handle.get_conversation().await;
            assert_eq!(
                head_text(&conv).as_deref(),
                Some("parent verbatim"),
                "preserve_inherited_system must not overwrite the inherited head"
            );
        })
        .await;
}
