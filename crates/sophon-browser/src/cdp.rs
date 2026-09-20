//! One multiplexed connection. Commands are sent once, never replayed.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::Error;

type Reply = oneshot::Sender<Result<Value, Error>>;

pub(crate) struct Cdp {
    tx: mpsc::Sender<(Value, Reply)>,
    task: JoinHandle<()>,
    pub events: Arc<Mutex<VecDeque<Value>>>,
    pub tabs: Arc<Mutex<HashMap<String, String>>>,
}

impl Cdp {
    pub async fn connect(url: &str, frames: broadcast::Sender<Value>) -> Result<Self, Error> {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let (mut writer, mut reader) = socket.split();
        let (tx, mut rx) = mpsc::channel::<(Value, Reply)>(32);
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let log = events.clone();
        let tabs = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let tab_ids = tabs.clone();
        let task = tokio::spawn(async move {
            let mut pending: HashMap<u64, Reply> = HashMap::new();
            let mut next = 0u64;
            let mut sequence = 0u64;
            loop {
                pending.retain(|_, reply| !reply.is_closed());
                tokio::select! {
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
                        } else if value["method"] == "Page.screencastFrame" {
                            // Frame traffic never shares the control-response queue or event log.
                            // ACK even with no subscribers; slow consumers receive Lagged explicitly.
                            next += 1;
                            let ack = json!({"id":next,"sessionId":value["sessionId"],"method":"Page.screencastFrameAck","params":{"sessionId":value["params"]["sessionId"]}});
                            if writer.send(Message::Text(ack.to_string().into())).await.is_err() { break; }
                            let tab = value["sessionId"].as_str().and_then(|session|tab_ids.lock().expect("tab lock").get(session).cloned());
                            if let Some(tab) = tab {
                                sequence += 1;
                                let _ = frames.send(json!({"tab_id":tab,"sequence":sequence,"mime_type":"image/jpeg","base64":value["params"]["data"],"metadata":value["params"]["metadata"]}));
                            }
                        } else if value.get("method").is_some() {
                            let mut log = log.lock().expect("event lock");
                            if log.len() == 512 { log.pop_front(); }
                            log.push_back(value);
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
            task,
            events,
            tabs,
        })
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

    pub async fn stop(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
