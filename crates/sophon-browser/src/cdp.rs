//! One multiplexed connection. Commands are sent once, never replayed.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::Error;
use crate::downloads::Downloads;

type Reply = oneshot::Sender<Result<Value, Error>>;

pub(crate) struct Cdp {
    tx: mpsc::Sender<(Value, Reply)>,
    task: Mutex<Option<JoinHandle<()>>>,
    pub input_generation: AtomicU64,
    pub events: Arc<Mutex<VecDeque<Value>>>,
    pub tabs: Arc<Mutex<HashMap<String, String>>>,
    pub latest_frames: Arc<Mutex<HashMap<String, Value>>>,
    pub downloads: Arc<Mutex<Downloads>>,
    ready_loaders: watch::Receiver<HashMap<String, String>>,
}

impl Cdp {
    pub async fn connect(
        url: &str,
        frames: broadcast::Sender<Value>,
        download_dir: std::path::PathBuf,
    ) -> Result<Self, Error> {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let (mut writer, mut reader) = socket.split();
        let (tx, mut rx) = mpsc::channel::<(Value, Reply)>(32);
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let log = events.clone();
        let tabs = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let tab_ids = tabs.clone();
        let latest_frames = Arc::new(Mutex::new(HashMap::new()));
        let latest = latest_frames.clone();
        let downloads = Arc::new(Mutex::new(Downloads::new(download_dir)));
        let download_events = downloads.clone();
        let (ready, ready_loaders) = watch::channel(HashMap::<String, String>::new());
        let task = tokio::spawn(async move {
            let mut pending: HashMap<u64, Reply> = HashMap::new();
            let mut main_frames = HashMap::<String, String>::new();
            let mut next = 0u64;
            let mut sequence = 0u64;
            let mut deadlines = tokio::time::interval(Duration::from_secs(1));
            loop {
                pending.retain(|_, reply| !reply.is_closed());
                tokio::select! {
                    _ = deadlines.tick() => {
                        let commands = download_events.lock().expect("download lock").expired();
                        for mut command in commands {
                            next += 1;
                            command["id"] = json!(next);
                            if writer.send(Message::Text(command.to_string().into())).await.is_err() { return; }
                        }
                    }
                    command = rx.recv() => {
                        let Some((mut command, reply)) = command else { break };
                        if reply.is_closed() { continue; }
                        next += 1;
                        command["id"] = json!(next);
                        pending.insert(next, reply);
                        if writer.send(Message::Text(command.to_string().into())).await.is_err() { break; }
                    }
                    incoming = reader.next() => {
                        let Some(Ok(message)) = incoming else { break };
                        let Message::Text(text) = message else { continue };
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                        if let Some(id) = value["id"].as_u64() {
                            if let Some(reply) = pending.remove(&id) {
                                let result = if value.get("error").is_some() {
                                    Err(Error::Protocol(value["error"].to_string()))
                                } else { Ok(value["result"].clone()) };
                                let _ = reply.send(result);
                            }
                        } else if value["method"] == "Browser.downloadWillBegin" || value["method"] == "Browser.downloadProgress" {
                            let commands = download_events.lock().expect("download lock").event(&value);
                            for mut command in commands {
                                next += 1;
                                command["id"] = json!(next);
                                if writer.send(Message::Text(command.to_string().into())).await.is_err() { return; }
                            }
                        } else if value["method"] == "Page.screencastFrame" {
                            // Frame traffic never shares the control-response queue or event log.
                            // ACK even with no subscribers; slow consumers receive Lagged explicitly.
                            next += 1;
                            let ack = json!({"id":next,"sessionId":value["sessionId"],"method":"Page.screencastFrameAck","params":{"sessionId":value["params"]["sessionId"]}});
                            if writer.send(Message::Text(ack.to_string().into())).await.is_err() { break; }
                            let tab = value["sessionId"].as_str().and_then(|session|tab_ids.lock().expect("tab lock").get(session).cloned());
                            if let Some(tab) = tab {
                                sequence += 1;
                                let frame = json!({"tab_id":tab,"sequence":sequence,"mime_type":"image/jpeg","base64":value["params"]["data"],"metadata":value["params"]["metadata"]});
                                latest.lock().expect("frame lock").insert(tab,frame.clone());
                                let _ = frames.send(frame);
                            }
                        } else if value.get("method").is_some() {
                            if value["method"] == "Page.frameNavigated"
                                && value["params"]["frame"].get("parentId").is_none()
                                && let (Some(session), Some(frame)) = (value["sessionId"].as_str(), value["params"]["frame"]["id"].as_str()) {
                                main_frames.insert(session.into(), frame.into());
                            }
                            if value["method"] == "Page.lifecycleEvent"
                                && value["params"]["name"] == "DOMContentLoaded"
                                && let (Some(session), Some(loader)) = (value["sessionId"].as_str(), value["params"]["loaderId"].as_str())
                                && main_frames.get(session).is_some_and(|frame| value["params"]["frameId"] == *frame) {
                                ready.send_modify(|loaders| { loaders.insert(session.into(), loader.into()); });
                            }
                            if value["method"] == "Target.detachedFromTarget"
                                && let Some(session) = value["params"]["sessionId"].as_str() {
                                if let Some(tab) = tab_ids.lock().expect("tab lock").remove(session) {
                                    latest.lock().expect("frame lock").remove(&tab);
                                }
                                main_frames.remove(session);
                                ready.send_modify(|loaders| { loaders.remove(session); });
                            }
                            let mut log = log.lock().expect("event lock");
                            if log.len() == 512 { log.pop_front(); }
                            if text.len()>64*1024 {
                                log.push_back(json!({"method":value["method"],"sessionId":value["sessionId"],"params":{"omitted":true,"reason":"event exceeds 64 KiB"}}));
                            } else { log.push_back(value); }
                        }
                    }
                }
            }
            for (_, reply) in pending {
                let _ = reply.send(Err(Error::Transport(
                    "CDP disconnected; operation outcome may be unknown".into(),
                )));
            }
        });
        Ok(Self {
            tx,
            task: Mutex::new(Some(task)),
            input_generation: AtomicU64::new(0),
            events,
            tabs,
            latest_frames,
            downloads,
            ready_loaders,
        })
    }

    /// Page.navigate can reply while its old RenderFrameHost is inactive. Wait
    /// for this exact document's DOM readiness before exposing it to reads.
    /// This never resends navigation or depends on the lossy diagnostic log.
    pub async fn wait_for_document(&self, session: &str, loader: &str) -> Result<(), Error> {
        let mut ready = self.ready_loaders.clone();
        tokio::time::timeout(Duration::from_secs(20), async {
            ready
                .wait_for(|loaders| loaders.get(session).is_some_and(|id| id == loader))
                .await
                .map(|_| ())
                .map_err(|_| Error::Closed)
        })
        .await
        .map_err(|_| Error::Timeout)?
    }

    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, Error> {
        let mut command = json!({"method":method,"params":params});
        if let Some(session) = session {
            command["sessionId"] = json!(session);
        }
        let (tx, rx) = oneshot::channel();
        tokio::time::timeout(Duration::from_secs(20), async {
            self.tx
                .send((command, tx))
                .await
                .map_err(|_| Error::Closed)?;
            rx.await.map_err(|_| Error::Closed)?
        })
        .await
        .map_err(|_| Error::Timeout)?
    }

    pub async fn stop(&self) {
        let task = self.task.lock().expect("task lock").take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().expect("task lock").take() {
            task.abort();
        }
    }
}
