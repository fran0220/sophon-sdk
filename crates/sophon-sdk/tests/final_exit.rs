#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use sophon_sdk::{
    Agent, AgentConfig, ClientHandler, Error, ModelConfig, PermissionPolicy, ProviderConfig,
    SessionConfig, StopReason,
};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use xai_grok_test_support::{EnvGuard, MockInferenceServer, ScriptedResponse, SseEvent};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(25), future)
        .await
        .expect("final-exit test timed out")
}

struct Questions {
    entered: tokio::sync::Notify,
    released: Arc<AtomicBool>,
}
struct Release(Arc<AtomicBool>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ClientHandler for Questions {
    async fn extension(&self, method: &str, _: Value) -> Result<Value, Error> {
        if method != "x.ai/ask_user_question" {
            return Err(Error::UnsupportedClientRequest(method.into()));
        }
        let _release = Release(self.released.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}

fn tool(
    server: &MockInferenceServer,
    id: &str,
    name: &str,
    arguments: Value,
) -> xai_grok_test_support::InferenceExpectation {
    let chunk = |delta: Value, reason: Value| json!({"id":"exit-test", "object":"chat.completion.chunk", "created":1234567890, "model":"wire-model", "choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
    server.expect_response(id, xai_grok_test_support::InferenceRequestMatcher::foreground(xai_grok_test_support::InferenceEndpoint::ChatCompletions), ScriptedResponse::sse(vec![
        SseEvent::data(chunk(json!({"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]}), Value::Null).to_string()),
        SseEvent::data(chunk(json!({}), json!("tool_calls")).to_string()),
        SseEvent::data("[DONE]".to_owned()),
    ]))
}

#[test]
fn final_exit_releases_real_question_cancels_queue_and_reaps_running_process() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
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
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
        let server = MockInferenceServer::start().await.unwrap();
        server.set_response("native tool complete");
        let questions = Arc::new(Questions { entered: tokio::sync::Notify::new(), released: Arc::new(AtomicBool::new(false)) });
        let agent = bounded(Agent::start(AgentConfig::new(ModelConfig::new("exit-model", ProviderConfig::openai_chat(server.url(), "test-key", "wire-model")))
            .permission_policy(PermissionPolicy::AllowAll).client_handler(questions.clone()))).await.unwrap();
        let session = bounded(agent.create_session(SessionConfig::new(workspace.path()))).await.unwrap();
        let pidfile = workspace.path().join("exit-child.pid");
        let _process_turn = tool(&server, "running-process", "run_terminal_command", json!({"command":format!("echo $$ > '{}'; exec /bin/sleep 600", pidfile.display()),"description":"final exit process lifecycle test","is_background":true}));
        bounded(session.prompt("start the test background process")).await.unwrap();
        let pid: u32 = bounded(async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = text.trim().parse() {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        let foreground_session = bounded(agent.create_session(SessionConfig::new(workspace.path()))).await.unwrap();
        let foreground_pidfile = workspace.path().join("exit-foreground.pid");
        let _foreground_turn = tool(&server, "foreground-process", "run_terminal_command", json!({"command":format!("echo $$ > '{}'; exec /bin/sleep 600", foreground_pidfile.display()),"description":"final exit foreground lifecycle test","is_background":false}));
        let foreground = { let session = foreground_session.clone(); tokio::spawn(async move { session.prompt("run the foreground command").await }) };
        let foreground_pid: u32 = bounded(async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&foreground_pidfile)
                    && let Ok(pid) = text.trim().parse() {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        let _question_turn = tool(&server, "pending-question", "ask_user_question", json!({"questions":[{"question":"Wait for a human answer","options":[{"label":"Yes","description":"answer"},{"label":"No","description":"decline"}]}]}));
        let running = { let session = session.clone(); tokio::spawn(async move { session.prompt("ask the question now").await }) };
        bounded(questions.entered.notified()).await;
        assert!(!questions.released.load(Ordering::SeqCst));
        let queued = { let session = session.clone(); tokio::spawn(async move { session.prompt("queued work must be cancelled").await }) };
        bounded(async { while session.queue_snapshot().await.unwrap().pending.is_empty() { tokio::task::yield_now().await; } }).await;
        let report = bounded(agent.quiesce(Duration::from_millis(50))).await.unwrap();
        assert!(!report.drained(), "plain quiesce must not pretend an unanswered question drained");
        bounded(agent.final_exit(Duration::from_secs(20))).await.unwrap();
        assert_eq!(agent.runtime_health().state, sophon_sdk::management::RuntimeState::Stopped);
        assert!(questions.released.load(Ordering::SeqCst), "pending callback future must be dropped");
        assert_eq!(bounded(running).await.unwrap().unwrap().stop_reason, StopReason::Cancelled);
        assert_eq!(bounded(queued).await.unwrap().unwrap().stop_reason, StopReason::Cancelled);
        assert_eq!(bounded(foreground).await.unwrap().unwrap().stop_reason, StopReason::Cancelled);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "native process must be reaped before final exit success");
        assert!(!std::path::Path::new(&format!("/proc/{foreground_pid}")).exists(), "foreground process must be reaped before final exit success");
        let info = xai_grok_shell::session::info::Info { id: agent_client_protocol::SessionId::new(session.id().as_str().to_owned()), cwd: workspace.path().to_str().unwrap().into() };
        let dir = xai_grok_shell::session::persistence::session_dir(&info);
        let snapshot = sophon_sdk::PortableSession::from_native_persistence(&dir, session.id().as_str(), &info.cwd).unwrap();
        let payload = serde_json::to_value(snapshot).unwrap();
        let events = payload["files"]["updates.jsonl"].as_str().unwrap();
        assert!(events.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).any(|event| {
            let event = event.get("params").unwrap_or(&event);
            event["update"]["sessionUpdate"] == "turn_completed" && event["update"]["stop_reason"] == "cancelled"
        }), "native cancellation terminal must survive checked final flush");
    });
}
