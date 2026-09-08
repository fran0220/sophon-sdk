use std::{future::Future, time::Duration};

use sophon_sdk::{
    Agent, AgentConfig, ModelConfig, PortabilityError, PortableSession, ProviderConfig, Session,
    SessionConfig, SessionId,
};
use xai_grok_test_support::{EnvGuard, MockInferenceServer};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("portable operation timed out")
}

async fn capture(session: &Session) -> PortableSession {
    bounded(async {
        loop {
            match session.export_portable().await {
                Ok(snapshot) => return snapshot,
                Err(PortabilityError::Busy) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(error) => panic!("export failed: {error}"),
            }
        }
    })
    .await
}

#[test]
fn portable_conversation_roundtrip_keeps_actor_usable_and_continues_native_context() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("GROK_HOME", home.path());
    let _telemetry = EnvGuard::set("GROK_TELEMETRY_ENABLED", "false");
    let _trace = EnvGuard::set("GROK_TRACE_UPLOAD", "false");
    let _feedback = EnvGuard::set("GROK_FEEDBACK_ENABLED", "false");
    let _summary = EnvGuard::set("GROK_TURN_SUMMARY", "false");
    std::fs::write(
        home.path().join("config.toml"),
        "[features]\nsession_recap = false\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("managed_config.toml"),
        "plugin_auto_update = false\n",
    )
    .unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let server = MockInferenceServer::start().await.unwrap();
            server.set_keep_requests(true);
            server.set_response("portable-native-answer");
            let config = AgentConfig::new(ModelConfig::new(
                "portable-model",
                ProviderConfig::openai_chat(server.url(), "local-only-credential", "wire-model"),
            ));
            let agent = bounded(Agent::start(config.clone())).await.unwrap();
            let session = bounded(agent.create_session(SessionConfig::new(source.path())))
                .await
                .unwrap();
            server.hold_agent_completions();
            let running = {
                let session = session.clone();
                tokio::spawn(async move { session.prompt("portable-original-question").await })
            };
            bounded(async {
                while session.queue_snapshot().await.unwrap().running.is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(matches!(
                session.export_portable().await,
                Err(PortabilityError::Busy)
            ));
            server.release_agent_completions();
            bounded(running).await.unwrap().unwrap();
            let snapshot = capture(&session).await;
            let bytes = snapshot.to_vec().unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("local-only-credential"));
            assert_eq!(snapshot.session_id(), session.id().as_str());
            assert!(snapshot.completeness().current_native_conversation);
            assert!(!snapshot.completeness().execution_custody);
            let snapshot = PortableSession::from_slice(&bytes).unwrap();
            assert!(matches!(
                agent
                    .import_portable(snapshot.clone(), destination.path().into())
                    .await,
                Err(PortabilityError::ActiveSession(_))
            ));
            let mut projection = agent.subscribe();
            let (concurrent_capture, later_prompt) = bounded(async {
                tokio::join!(
                    session.history_snapshot(),
                    session.prompt("source-only-later-turn")
                )
            })
            .await;
            let concurrent_capture = concurrent_capture.unwrap();
            assert!(concurrent_capture.records.iter().all(|record| record.is_replay));
            assert!(concurrent_capture.records.iter().any(|record| matches!(&record.update, sophon_sdk::SessionUpdate::UserText(text) if text == "portable-original-question")));
            let native_user = concurrent_capture.records.iter().find(|record| matches!(&record.update, sophon_sdk::SessionUpdate::UserText(text) if text == "portable-original-question")).unwrap();
            assert!(native_user.event_id.is_some());
            assert!(native_user.prompt_index.is_some());
            assert!(!concurrent_capture.records.iter().any(|record| matches!(&record.update, sophon_sdk::SessionUpdate::UserText(text) if text == "source-only-later-turn")));
            later_prompt.unwrap();
            let mut boundary_seen = false;
            let mut later_echoes = 0;
            loop {
                let event = match projection.try_recv() {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(error) => panic!("history stream invalidated: {error}"),
                };
                match event {
                    sophon_sdk::Event::HistoryBoundary { boundary_id, .. } if boundary_id == concurrent_capture.boundary_id => boundary_seen = true,
                    sophon_sdk::Event::HistoryRecord(record) if matches!(&record.update, sophon_sdk::SessionUpdate::UserText(text) if text == "source-only-later-turn") => {
                        assert!(boundary_seen, "live echo must follow acknowledged snapshot boundary");
                        assert!(!record.is_replay);
                        later_echoes += 1;
                    }
                    _ => {}
                }
            }
            assert!(boundary_seen);
            assert_eq!(later_echoes, 1);
            assert_ne!(capture(&session).await.revision(), snapshot.revision());
            // Dropping the caller does not leave a permanent admission fence.
            let cancelled = {
                let session = session.clone();
                tokio::spawn(async move { session.export_portable().await })
            };
            tokio::task::yield_now().await;
            cancelled.abort();
            let _ = cancelled.await;
            bounded(session.set_mode("default")).await.unwrap();
            capture(&session).await;
            bounded(agent.shutdown()).await.unwrap();

            let source_info = xai_grok_shell::session::info::Info {
                id: agent_client_protocol::SessionId::new(snapshot.session_id().to_owned()),
                cwd: source.path().to_str().unwrap().into(),
            };
            let source_dir = xai_grok_shell::session::persistence::session_dir(&source_info);
            // No Agent is running here: offline inspection cannot load old cwd,
            // providers or pending tool work. Native persisted data is sufficient.
            let offline = PortableSession::from_native_persistence(&source_dir, snapshot.session_id(), &source_info.cwd).unwrap();
            assert_eq!(offline.session_id(), snapshot.session_id());
            assert!(String::from_utf8_lossy(&offline.to_vec().unwrap()).contains("source-only-later-turn"));
            std::fs::rename(&source_dir, home.path().join("source-archive")).unwrap();
            let agent = bounded(Agent::start(config)).await.unwrap();
            let destination_info = xai_grok_shell::session::info::Info {
                id: source_info.id.clone(),
                cwd: destination.path().to_str().unwrap().into(),
            };
            let destination_dir =
                xai_grok_shell::session::persistence::session_dir(&destination_info);
            for version in [0, 2] {
                let mut value = serde_json::to_value(&snapshot).unwrap();
                value["format_version"] = version.into();
                let incompatible: PortableSession = serde_json::from_value(value).unwrap();
                assert!(matches!(
                    agent
                        .import_portable(incompatible, destination.path().into())
                        .await,
                    Err(PortabilityError::Incompatible)
                ));
                assert!(!destination_dir.exists());
            }
            let mut malformed = serde_json::to_value(&snapshot).unwrap();
            malformed["files"]["chat_history.jsonl"] = "invalid native JSON".into();
            let malformed: PortableSession = serde_json::from_value(malformed).unwrap();
            assert!(matches!(
                agent
                    .import_portable(malformed, destination.path().into())
                    .await,
                Err(PortabilityError::Malformed(_))
            ));
            assert!(!destination_dir.exists());
            assert_eq!(
                agent
                    .portable_import_status(snapshot.clone(), destination.path().into())
                    .await
                    .unwrap(),
                sophon_sdk::PortableImportStatus::Missing
            );

            let before_requests = server.request_count_for("/v1/chat/completions");
            let (first_import, racing_import) = bounded(async {
                tokio::join!(
                    agent.import_portable(snapshot.clone(), destination.path().into()),
                    agent.import_portable(snapshot.clone(), destination.path().into())
                )
            })
            .await;
            let id = first_import.unwrap();
            assert!(matches!(
                racing_import,
                Err(PortabilityError::ExistingSession(_))
            ));
            assert_eq!(id, SessionId::from(snapshot.session_id()));
            assert_eq!(
                server.request_count_for("/v1/chat/completions"),
                before_requests,
                "import must not run agent actions"
            );
            let original_chat = std::fs::read(destination_dir.join("chat_history.jsonl")).unwrap();
            let original_updates = std::fs::read(destination_dir.join("updates.jsonl")).unwrap();
            assert_eq!(
                agent
                    .portable_import_status(snapshot.clone(), destination.path().into())
                    .await
                    .unwrap(),
                sophon_sdk::PortableImportStatus::MatchesSnapshot
            );
            // Not a stale receipt: a later valid native metadata change must be
            // detected even though the Session ID has not changed.
            let summary_path = destination_dir.join("summary.json");
            let original_summary = std::fs::read(&summary_path).unwrap();
            let mut changed: serde_json::Value = serde_json::from_slice(&original_summary).unwrap();
            changed["session_summary"] = "unrelated local title change".into();
            std::fs::write(&summary_path, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert_eq!(
                agent
                    .portable_import_status(snapshot.clone(), destination.path().into())
                    .await
                    .unwrap(),
                sophon_sdk::PortableImportStatus::Different
            );
            std::fs::write(&summary_path, original_summary).unwrap();
            assert!(matches!(
                agent
                    .import_portable(snapshot.clone(), destination.path().into())
                    .await,
                Err(PortabilityError::ExistingSession(_))
            ));
            assert_eq!(
                std::fs::read(destination_dir.join("chat_history.jsonl")).unwrap(),
                original_chat
            );
            assert_eq!(
                std::fs::read(destination_dir.join("updates.jsonl")).unwrap(),
                original_updates
            );
            for excluded in [
                "rewind_points.jsonl",
                "resources_state.json",
                "tool_state.json",
                "goal",
                "workflows",
            ] {
                assert!(!destination_dir.join(excluded).exists());
            }
            let restored = bounded(agent.load_session(id, SessionConfig::new(destination.path())))
                .await
                .unwrap();
            let history = bounded(restored.history_snapshot()).await.unwrap();
            assert!(history.records.iter().any(|record| matches!(&record.update, sophon_sdk::SessionUpdate::AssistantText(text) if text.contains("portable-native-answer"))));
            assert!(history.records.iter().all(|record| record.is_replay));
            let repeated = bounded(restored.history_snapshot()).await.unwrap();
            assert_eq!(history.revision, repeated.revision);
            assert_ne!(history.boundary_id, repeated.boundary_id);
            assert_eq!(server.request_count_for("/v1/chat/completions"), before_requests, "history capture/replay must not start inference");
            assert!(matches!(
                agent
                    .portable_import_status(snapshot.clone(), destination.path().into())
                    .await,
                Err(PortabilityError::ActiveSession(_))
            ));
            bounded(restored.prompt("continue-after-portable-import"))
                .await
                .unwrap();
            let requests = server.request_bodies();
            let continuation = requests
                .iter()
                .find(|request| {
                    request
                        .to_string()
                        .contains("continue-after-portable-import")
                })
                .expect("continuation inference request");
            let context = continuation.to_string();
            assert!(context.contains("portable-original-question"));
            assert!(context.contains("portable-native-answer"));
            assert!(!context.contains("source-only-later-turn"));
            bounded(agent.shutdown()).await.unwrap();
        });
}
