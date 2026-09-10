//! Bounded observation of the existing native MCP generation. Never starts or restarts init.
use super::*;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitRequest {
    session_id: String,
    timeout_ms: u64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ready,
    Failed,
    Replaced,
    TimedOut,
    NotStarted,
    Abandoned,
}

#[derive(Debug, Serialize)]
struct WaitResponse {
    outcome: Outcome,
}

pub(super) async fn handle_wait_ready(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<WaitRequest>(args)?;
    if req.timeout_ms == 0 || req.timeout_ms > 60_000 {
        return Err(acp::Error::invalid_params().data("timeoutMs must be in 1..=60000"));
    }
    let id = acp::SessionId::new(req.session_id);
    let handle = agent
        .get_session_handle(&id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    // The actor already hands out status snapshots. Carry its native Arc through that
    // existing command instead of adding a registry or mistaking the agent pool for it.
    let outcome = tokio::time::timeout(Duration::from_millis(req.timeout_ms), async {
        let snapshot = handle.get_mcp_status().await;
        let state = snapshot.state.ok_or_else(|| {
            acp::Error::internal_error().data("session MCP authority unavailable")
        })?;
        Ok::<_, acp::Error>(observe(&state).await)
    })
    .await
    .unwrap_or(Ok(Outcome::TimedOut))?;
    to_ext_response(Ok(WaitResponse { outcome }))
}

async fn observe(state: &Arc<TokioMutex<McpState>>) -> Outcome {
    let generation = state.lock().await.current_generation();
    loop {
        let (complete, initializing, abandoned, failed, clients, names) = {
            let state = state.lock().await;
            if generation.is_cancelled() {
                return Outcome::Replaced;
            }
            (
                state.is_initialized(),
                state.is_initializing(),
                state.is_init_abandoned(),
                !state.init_failed.is_empty() || !state.auth_required.is_empty(),
                state
                    .all_clients()
                    .map(|(_, c)| c.clone())
                    .collect::<Vec<_>>(),
                state
                    .configs
                    .iter()
                    .map(|c| crate::session::mcp_servers::mcp_server_name(c).to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        if abandoned {
            return Outcome::Abandoned;
        }
        if complete {
            let mut healthy = !failed;
            for name in names {
                healthy &= clients.iter().any(|c| c.server_name() == name);
            }
            // Include in-process ACP clients too, not just disk transport configs.
            for client in clients {
                match generation.or_cancel(client.is_healthy()).await {
                    Ok(is_healthy) => healthy &= is_healthy,
                    Err(_) => return Outcome::Replaced,
                }
            }
            // A completed old generation must never certify its replacement.
            let state = state.lock().await;
            if generation.is_cancelled() {
                return Outcome::Replaced;
            }
            healthy &= state.is_initialized()
                && state.init_failed.is_empty()
                && state.auth_required.is_empty();
            return if healthy {
                Outcome::Ready
            } else {
                Outcome::Failed
            };
        }
        if !initializing {
            return Outcome::NotStarted;
        }
        if generation
            .or_cancel(tokio::time::sleep(Duration::from_millis(20)))
            .await
            .is_err()
        {
            return Outcome::Replaced;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn not_started_complete_failed_and_replaced_are_distinct() {
        let state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        assert_eq!(observe(&state).await, Outcome::NotStarted);
        let claim = state.lock().await.try_start_init().unwrap();
        // An owned pass is not ready, even if there are no configured servers.
        assert!(
            tokio::time::timeout(Duration::from_millis(5), observe(&state))
                .await
                .is_err()
        );
        let observing = observe(&state);
        let replace = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            state.lock().await.restart_init()
        };
        let (outcome, successor) = tokio::join!(observing, replace);
        assert_eq!(outcome, Outcome::Replaced);
        drop(claim);
        state.lock().await.finish_init();
        // finish_init only releases the actor; publication/handshakes are not complete.
        assert!(
            tokio::time::timeout(Duration::from_millis(5), observe(&state))
                .await
                .is_err()
        );
        state.lock().await.complete_init();
        assert_eq!(observe(&state).await, Outcome::Ready);
        state.lock().await.auth_required.insert("needs-auth".into());
        assert_eq!(observe(&state).await, Outcome::Failed);
        drop(successor);
        drop(state.lock().await.restart_init());
        assert_eq!(observe(&state).await, Outcome::Abandoned);
    }
}
