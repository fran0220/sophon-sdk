//! Runtime-local, Electron-independent browser execution. No CLI, MCP or agent loop.
mod cdp;
mod recording;

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use cdp::Cdp;
use recording::Recording;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, broadcast};
use uuid::Uuid;
use xai_tty_utils::{ProcessGroup, ProcessScope};

const AGENT_ACTIONS: &[&str] = &[
    "capabilities",
    "tabs",
    "new_tab",
    "close_tab",
    "navigate",
    "frames",
    "snapshot",
    "click",
    "type",
    "key",
    "scroll",
    "wait",
    "screenshot",
    "events",
    "record_start",
    "record_stop",
];

#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub executable: PathBuf,
    /// Dedicated account/Runtime directory, shared across Games. Never delete on
    /// Game removal and never point at a user's default Chrome profile.
    pub data_dir: PathBuf,
    /// Durable Runtime-owned artifact store, independent of browser identity.
    pub artifact_dir: PathBuf,
    pub headless: bool,
    /// Explicit opt-in for isolated containers only. Never enabled implicitly.
    pub no_sandbox: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("browser closed")]
    Closed,
    #[error(
        "browser operation timed out; effects may already have occurred; do not replay automatically"
    )]
    Timeout,
    #[error("stale element ref: take a fresh snapshot")]
    StaleRef,
    #[error("unsupported browser capability: {0}")]
    Unsupported(String),
    #[error("invalid browser request: {0}")]
    Invalid(String),
    #[error("browser transport: {0}")]
    Transport(String),
    #[error("CDP: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Closed => "browser_closed",
            Self::Timeout => "outcome_unknown",
            Self::StaleRef => "stale_ref",
            Self::Unsupported(_) => "unsupported_capability",
            Self::Invalid(_) => "invalid_request",
            Self::Transport(_) => "transport_error",
            Self::Protocol(_) => "cdp_error",
            Self::Io(_) => "io_error",
        }
    }
}

struct ElementRef {
    backend: i64,
    context: i64,
    revision: u64,
    input_generation: u64,
}
struct Page {
    session: String,
    refs: HashMap<String, ElementRef>,
    streaming: bool,
}
struct Running {
    child: Child,
    group: Option<Arc<ProcessGroup>>,
    cdp: Option<Arc<Cdp>>,
    pages: HashMap<String, Page>,
    recording: Option<Recording>,
    _profile_lock: File,
}

/// Operations are serialized; dropping a call stops waiting, never retries it.
/// Call `close().await` for checked process cleanup; Drop is only a kill fallback.
pub struct BrowserService {
    config: BrowserConfig,
    process_scope: ProcessScope,
    running: Mutex<Option<Running>>,
    connection: std::sync::RwLock<Option<Arc<Cdp>>>,
    closed: AtomicBool,
    closing: Notify,
    frames: broadcast::Sender<Value>,
}

impl BrowserService {
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            config,
            process_scope: ProcessScope::new(),
            running: Mutex::new(None),
            connection: std::sync::RwLock::new(None),
            closed: AtomicBool::new(false),
            closing: Notify::new(),
            frames: broadcast::channel(8).0,
        }
    }

    /// Live JPEG frames only, separate from reliable control responses. A slow
    /// receiver gets `Lagged` and should resume with the newest available frame.
    pub fn subscribe_frames(&self) -> broadcast::Receiver<Value> {
        self.frames.subscribe()
    }

    pub fn tool_specs() -> Vec<BrowserToolSpec> {
        vec![BrowserToolSpec {
            name: "browser".into(),
            description: "Use the Runtime-owned browser. Snapshot gives revision-scoped refs; refresh after page changes. Never retry a timed-out interaction automatically. Screenshots and silent video recordings return durable artifact IDs. Audio unsupported.".into(),
            input_schema: json!({"type":"object","required":["action"],"properties":{
                "action":{"type":"string","enum":AGENT_ACTIONS},
                "tab_id":{"type":"string"},"frame_id":{"type":"string"},"url":{"type":"string"},"ref":{"type":"string"},"text":{"type":"string"},"key":{"type":"string"},"delta_x":{"type":"number"},"delta_y":{"type":"number"},"milliseconds":{"type":"integer","minimum":0,"maximum":10000}
            },"additionalProperties":false}),
        }]
    }

    pub async fn execute(&self, name: &str, args: Value) -> Result<Value, Error> {
        if name != "browser" {
            return Err(Error::Invalid(format!("unknown tool {name}")));
        }
        let action = string(&args, "action")?;
        if !AGENT_ACTIONS.contains(&action) {
            return Err(Error::Unsupported(format!(
                "unregistered agent action {action}"
            )));
        }
        self.execute_host(args).await
    }

    /// Trusted Runtime host boundary for human input, Stage probes and explicit
    /// Settings operations. Never register this entry point as an agent tool.
    pub async fn execute_host(&self, args: Value) -> Result<Value, Error> {
        let closing = self.closing.notified();
        tokio::pin!(closing);
        closing.as_mut().enable();
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        tokio::select! {
            biased;
            _ = closing => Err(Error::Closed),
            result = tokio::time::timeout(Duration::from_secs(60), self.execute_inner(args)) => result.map_err(|_| Error::Timeout)?,
        }
    }

    async fn execute_inner(&self, args: Value) -> Result<Value, Error> {
        let action = string(&args, "action")?;
        if action == "capabilities" {
            return Ok(
                json!({"engine":"chromium-cdp","persistent_profile":true,"semantic_snapshots":true,"frame_snapshots":true,"screenshots":true,"input":true,"console":true,"network_metadata":true,"streaming":true,"audio":false,"recording":"requires_ffmpeg","cross_origin_frame_snapshots":false}),
            );
        }
        if matches!(action, "stream" | "audio" | "record" | "recording") {
            return Err(Error::Unsupported(action.into()));
        }
        if action == "artifact" {
            let id = string(&args, "artifact_id")?;
            let (path, mime) = self.artifact_path(id).await?;
            if tokio::fs::metadata(&path).await?.len() > 64 * 1024 * 1024 {
                return Err(Error::Invalid(
                    "artifact exceeds 64 MiB inline transfer limit".into(),
                ));
            }
            let bytes = tokio::fs::read(path).await?;
            return Ok(
                json!({"mime_type":mime,"base64":base64::engine::general_purpose::STANDARD.encode(bytes)}),
            );
        }
        if action == "release_artifact" {
            // Durable evidence is retained. Clients only release their own references.
            return Ok(json!({"released":true,"retained":true}));
        }
        // Human input must not queue behind an agent wait, long probe or video
        // encoding. Only first attachment/startup goes through the lifecycle lock.
        if action == "input" {
            let cdp = self.connection.read().expect("connection lock").clone();
            if let Some(cdp) = cdp {
                let tab = string(&args, "tab_id")?;
                let session = cdp
                    .tabs
                    .lock()
                    .expect("tab lock")
                    .iter()
                    .find(|(_, id)| id.as_str() == tab)
                    .map(|(session, _)| session.clone());
                if let Some(session) = session {
                    return host_input(&cdp, Some(&session), &args).await;
                }
            }
        }
        let mut guard = self.running.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if guard.is_none() {
            *guard = Some(self.launch().await?);
        }
        if action == "clear_profile" {
            self.connection.write().expect("connection lock").take();
            stop_running(guard.as_mut().expect("launched")).await?;
            // Keep the exclusive profile lock until every old context has exited
            // and the identity directory is gone. Artifacts are not identity.
            tokio::fs::remove_dir_all(self.config.data_dir.join("profile")).await?;
            guard.take();
            return Ok(json!({"cleared":true,"closed_all_tabs":true,"artifacts_retained":true}));
        }
        if action == "clear_site" {
            let origin = site_origin(string(&args, "origin")?)?;
            let old = guard.as_mut().expect("launched");
            let lock = old._profile_lock.try_clone()?;
            self.connection.write().expect("connection lock").take();
            stop_running(old).await?;
            // Restart with the same lock so old windows/workers cannot race the
            // clear and repopulate storage. Other origins' saved data is kept.
            *guard = Some(self.launch_with_lock(lock).await?);
            let running = guard.as_mut().expect("restarted");
            running.cdp = Some(self.connect(&mut running.child).await?);
            let cdp = running.cdp.as_ref().expect("connected");
            let target = cdp
                .call(None, "Target.createTarget", json!({"url":"about:blank"}))
                .await?;
            let attached = cdp
                .call(
                    None,
                    "Target.attachToTarget",
                    json!({"targetId":target["targetId"],"flatten":true}),
                )
                .await?;
            cdp.call(
                Some(string(&attached, "sessionId")?),
                "Storage.clearDataForOrigin",
                json!({"origin":origin,"storageTypes":"all"}),
            )
            .await?;
            cdp.call(
                None,
                "Target.closeTarget",
                json!({"targetId":target["targetId"]}),
            )
            .await?;
            return Ok(json!({"cleared":true,"origin":origin,"closed_all_tabs":true}));
        }
        let running = guard.as_mut().expect("launched");
        if running.cdp.is_none() {
            running.cdp = Some(self.connect(&mut running.child).await?);
        }
        let connection = running.cdp.as_ref().expect("connected");
        match action {
            "tabs" => return connection.call(None, "Target.getTargets", json!({})).await.map(|v| json!({"tabs":v["targetInfos"].as_array().into_iter().flatten().filter(|t|t["type"] == "page").cloned().collect::<Vec<_>>()})),
            "new_tab" => {
                let url = args["url"].as_str().unwrap_or("about:blank");
                validate_url(url)?;
                let result = connection.call(None,"Target.createTarget",json!({"url":url})).await?;
                return Ok(json!({"tab_id":result["targetId"]}));
            }
            "close_tab" => {
                let tab = string(&args,"tab_id")?;
                if running.recording.as_ref().is_some_and(|r|r.tab==tab) { return Err(Error::Invalid("stop recording before closing its tab".into())); }
                let result = connection.call(None,"Target.closeTarget",json!({"targetId":tab})).await?;
                if let Some(page) = running.pages.remove(tab) { connection.tabs.lock().expect("tab lock").remove(&page.session); }
                connection.latest_frames.lock().expect("frame lock").remove(tab);
                return Ok(result);
            }
            _ => {}
        }
        let tab = string(&args, "tab_id")?;
        if !running.pages.contains_key(tab) {
            let attached = connection
                .call(
                    None,
                    "Target.attachToTarget",
                    json!({"targetId":tab,"flatten":true}),
                )
                .await?;
            let session = string(&attached, "sessionId")?.to_owned();
            connection
                .tabs
                .lock()
                .expect("tab lock")
                .insert(session.clone(), tab.into());
            for method in [
                "Page.enable",
                "Runtime.enable",
                "Network.enable",
                "Accessibility.enable",
            ] {
                connection.call(Some(&session), method, json!({})).await?;
            }
            running.pages.insert(
                tab.into(),
                Page {
                    session,
                    refs: HashMap::new(),
                    streaming: false,
                },
            );
        }
        let page = running.pages.get_mut(tab).expect("attached");
        let cdp = connection;
        let session = Some(page.session.as_str());
        match action {
            "viewport" => {
                let dimension = |key: &str| {
                    args[key]
                        .as_u64()
                        .filter(|n| (1..=4096).contains(n))
                        .ok_or_else(|| Error::Invalid(format!("{key} must be 1..4096")))
                };
                let width = dimension("width")?;
                let height = dimension("height")?;
                page.refs.clear();
                cdp.call(
                    session,
                    "Emulation.setDeviceMetricsOverride",
                    json!({"width":width,"height":height,"deviceScaleFactor":1,"mobile":false}),
                )
                .await?;
                Ok(json!({"width":width,"height":height,"device_scale_factor":1}))
            }
            "state" => page_state(cdp, session).await,
            "back" | "forward" | "reload" => {
                page.refs.clear();
                let history = cdp
                    .call(session, "Page.getNavigationHistory", json!({}))
                    .await?;
                let current = history["currentIndex"].as_i64().unwrap_or(0);
                let index = current
                    + match action {
                        "back" => -1,
                        "forward" => 1,
                        _ => 0,
                    };
                let entries = history["entries"]
                    .as_array()
                    .ok_or_else(|| Error::Protocol("missing navigation history".into()))?;
                let entry = usize::try_from(index)
                    .ok()
                    .and_then(|i| entries.get(i))
                    .ok_or_else(|| Error::Invalid(format!("cannot navigate {action}")))?;
                if action == "reload" {
                    cdp.call(session, "Page.reload", json!({})).await?;
                } else {
                    cdp.call(
                        session,
                        "Page.navigateToHistoryEntry",
                        json!({"entryId":entry["id"]}),
                    )
                    .await?;
                }
                Ok(
                    json!({"url":entry["url"],"title":entry["title"],"can_go_back":index>0,"can_go_forward":index+1<entries.len() as i64,"loading":true}),
                )
            }
            "stream_start" => {
                if !page.streaming {
                    page.streaming = true;
                    cdp.call(session,"Page.startScreencast",json!({"format":"jpeg","quality":80,"maxWidth":1920,"maxHeight":1080,"everyNthFrame":1})).await?;
                }
                Ok(json!({"streaming":true,"audio":false}))
            }
            "stream_stop" => {
                if running.recording.as_ref().is_some_and(|r| r.tab == tab) {
                    return Err(Error::Invalid(
                        "stop recording before stopping stream".into(),
                    ));
                }
                cdp.call(session, "Page.stopScreencast", json!({})).await?;
                page.streaming = false;
                Ok(json!({"streaming":false}))
            }
            "record_start" => {
                if running.recording.is_some() {
                    return Err(Error::Invalid("recording already active".into()));
                }
                let initial = cdp
                    .latest_frames
                    .lock()
                    .expect("frame lock")
                    .get(tab)
                    .cloned();
                running.recording = Some(
                    Recording::start(
                        tab.into(),
                        self.config.artifact_dir.clone(),
                        self.subscribe_frames(),
                        self.process_scope.clone(),
                        initial,
                    )
                    .await?,
                );
                if !page.streaming {
                    page.streaming = true;
                    cdp.call(session,"Page.startScreencast",json!({"format":"jpeg","quality":80,"maxWidth":1920,"maxHeight":1080,"everyNthFrame":1})).await?;
                }
                Ok(
                    json!({"recording_id":running.recording.as_ref().expect("recording").id,"audio":false}),
                )
            }
            "record_stop" => {
                let recording = running
                    .recording
                    .as_mut()
                    .filter(|r| r.tab == tab)
                    .ok_or_else(|| Error::Invalid("no recording for tab".into()))?;
                let id = recording.id.clone();
                let result = recording.finish().await;
                running.recording.take();
                result?;
                Ok(json!({"artifact_id":id,"mime_type":"video/mp4","audio":false}))
            }
            "evaluate" => {
                page.refs.clear();
                let result = cdp.call(session,"Runtime.evaluate",json!({"expression":string(&args,"expression")?,"returnByValue":true,"awaitPromise":true})).await?;
                if let Some(exception) = result.get("exceptionDetails") {
                    return Err(Error::Protocol(exception.to_string()));
                }
                Ok(result["result"].clone())
            }
            "navigate" => {
                let url = string(&args, "url")?;
                validate_url(url)?;
                page.refs.clear();
                let result = cdp
                    .call(session, "Page.navigate", json!({"url":url}))
                    .await?;
                if let Some(error) = result["errorText"].as_str() {
                    return Err(Error::Protocol(error.into()));
                }
                Ok(result)
            }
            "frames" => cdp.call(session, "Page.getFrameTree", json!({})).await,
            "snapshot" => {
                page.refs.clear();
                let input_generation = cdp.input_generation.load(Ordering::Acquire);
                let frame = if let Some(frame) = args["frame_id"].as_str() {
                    frame.to_owned()
                } else {
                    let tree = cdp.call(session, "Page.getFrameTree", json!({})).await?;
                    string(&tree["frameTree"]["frame"], "id")?.to_owned()
                };
                let world = cdp
                    .call(
                        session,
                        "Page.createIsolatedWorld",
                        json!({"frameId":frame,"worldName":"sophon-browser"}),
                    )
                    .await?;
                let context = world["executionContextId"]
                    .as_i64()
                    .ok_or_else(|| Error::Unsupported("frame execution context".into()))?;
                let revision = revision(cdp, session, context).await?;
                let tree = cdp
                    .call(
                        session,
                        "Accessibility.getFullAXTree",
                        json!({"frameId":frame}),
                    )
                    .await?;
                let snapshot = Uuid::new_v4().to_string();
                let mut nodes = Vec::new();
                for node in tree["nodes"].as_array().into_iter().flatten() {
                    if node["ignored"] == true {
                        continue;
                    }
                    let mut item = json!({"role":node["role"]["value"],"name":node["name"]["value"],"value":node["value"]["value"],"properties":node["properties"],"node_id":node["nodeId"],"children":node["childIds"]});
                    if let Some(backend) = node["backendDOMNodeId"].as_i64() {
                        let id = format!("{snapshot}:{backend}");
                        page.refs.insert(
                            id.clone(),
                            ElementRef {
                                backend,
                                context,
                                revision,
                                input_generation,
                            },
                        );
                        item["ref"] = json!(id);
                    }
                    nodes.push(item);
                }
                if revision != self::revision(cdp, session, context).await?
                    || input_generation != cdp.input_generation.load(Ordering::Acquire)
                {
                    page.refs.clear();
                    return Err(Error::StaleRef);
                }
                Ok(
                    json!({"snapshot_id":snapshot,"frame_id":frame,"revision":revision,"nodes":nodes}),
                )
            }
            "click" | "type" => {
                let reference = page
                    .refs
                    .get(string(&args, "ref")?)
                    .ok_or(Error::StaleRef)?;
                if reference.input_generation != cdp.input_generation.load(Ordering::Acquire) {
                    return Err(Error::StaleRef);
                }
                if revision(cdp, session, reference.context)
                    .await
                    .map_err(|_| Error::StaleRef)?
                    != reference.revision
                {
                    return Err(Error::StaleRef);
                }
                let node = cdp.call(session,"DOM.resolveNode",json!({"backendNodeId":reference.backend,"executionContextId":reference.context})).await.map_err(|_| Error::StaleRef)?;
                let backend = reference.backend;
                let expected_input_generation = reference.input_generation;
                let object = string(&node["object"], "objectId")?;
                let function = if action == "click" {
                    "function(revision){if(!this.isConnected||globalThis.__sophonRevision.n!==revision)throw Error('stale');this.scrollIntoView({block:'center',inline:'center'});return true}"
                } else {
                    "function(revision){if(!this.isConnected||globalThis.__sophonRevision.n!==revision)throw Error('stale');this.focus();return this.getRootNode().activeElement===this}"
                };
                let located = cdp.call(session,"Runtime.callFunctionOn",json!({"objectId":object,"functionDeclaration":function,"arguments":[{"value":reference.revision}],"returnByValue":true})).await?;
                let _ = cdp
                    .call(session, "Runtime.releaseObject", json!({"objectId":object}))
                    .await;
                if located.get("exceptionDetails").is_some() {
                    return Err(Error::StaleRef);
                }
                if expected_input_generation != cdp.input_generation.load(Ordering::Acquire) {
                    return Err(Error::StaleRef);
                }
                page.refs.clear();
                if action == "type" {
                    if located["result"]["value"] != true {
                        return Err(Error::Invalid("element cannot receive text focus".into()));
                    }
                    cdp.call(
                        session,
                        "Input.insertText",
                        json!({"text":string(&args,"text")?}),
                    )
                    .await
                } else {
                    // Chromium computes viewport coordinates across same-process
                    // frames and CSS transforms; do not add frame offsets by hand.
                    let layout = cdp
                        .call(
                            session,
                            "DOM.getContentQuads",
                            json!({"backendNodeId":backend}),
                        )
                        .await?;
                    let quad = layout["quads"]
                        .as_array()
                        .and_then(|quads| quads.first())
                        .and_then(Value::as_array)
                        .filter(|quad| quad.len() == 8)
                        .ok_or_else(|| Error::Invalid("element has no clickable quad".into()))?;
                    let coords = quad
                        .iter()
                        .map(Value::as_f64)
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| Error::Protocol("invalid layout quad".into()))?;
                    let area = (0..4)
                        .map(|i| {
                            coords[2 * i] * coords[(2 * ((i + 1) % 4)) + 1]
                                - coords[2 * ((i + 1) % 4)] * coords[2 * i + 1]
                        })
                        .sum::<f64>()
                        .abs()
                        / 2.0;
                    if area < 1.0 {
                        return Err(Error::Invalid("element has no clickable area".into()));
                    }
                    let x = (0..4).map(|i| coords[2 * i]).sum::<f64>() / 4.0;
                    let y = (0..4).map(|i| coords[2 * i + 1]).sum::<f64>() / 4.0;
                    if expected_input_generation != cdp.input_generation.load(Ordering::Acquire) {
                        return Err(Error::StaleRef);
                    }
                    for kind in ["mousePressed", "mouseReleased"] {
                        cdp.call(
                            session,
                            "Input.dispatchMouseEvent",
                            json!({"type":kind,"x":x,"y":y,"button":"left","clickCount":1}),
                        )
                        .await?;
                    }
                    Ok(json!({"ok":true}))
                }
            }
            "key" => {
                page.refs.clear();
                let key = string(&args, "key")?;
                let code = match key {
                    "Enter" => 13,
                    "Tab" => 9,
                    "Escape" => 27,
                    "Backspace" => 8,
                    "ArrowLeft" => 37,
                    "ArrowUp" => 38,
                    "ArrowRight" => 39,
                    "ArrowDown" => 40,
                    "Delete" => 46,
                    _ => return Err(Error::Invalid("unsupported key; use type for text".into())),
                };
                for kind in ["keyDown", "keyUp"] {
                    cdp.call(
                        session,
                        "Input.dispatchKeyEvent",
                        json!({"type":kind,"key":key,"windowsVirtualKeyCode":code}),
                    )
                    .await?;
                }
                Ok(json!({"ok":true}))
            }
            "scroll" => {
                page.refs.clear();
                cdp.call(session,"Input.dispatchMouseEvent",json!({"type":"mouseWheel","x":0,"y":0,"deltaX":args["delta_x"].as_f64().unwrap_or(0.0),"deltaY":args["delta_y"].as_f64().unwrap_or(0.0)})).await
            }
            "wait" => {
                let ms = args["milliseconds"]
                    .as_u64()
                    .filter(|ms| *ms <= 10000)
                    .ok_or_else(|| Error::Invalid("milliseconds must be 0..10000".into()))?;
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(json!({"waited_ms":ms}))
            }
            "screenshot" => {
                let image = cdp
                    .call(
                        session,
                        "Page.captureScreenshot",
                        json!({"format":"png","captureBeyondViewport":false}),
                    )
                    .await?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(string(&image, "data")?)
                    .map_err(|e| Error::Protocol(e.to_string()))?;
                let id = Uuid::new_v4().to_string();
                let len = bytes.len();
                tokio::fs::create_dir_all(&self.config.artifact_dir).await?;
                let staging = self.config.artifact_dir.join(format!("{id}.partial"));
                tokio::fs::write(&staging, bytes).await?;
                tokio::fs::rename(staging, self.config.artifact_dir.join(format!("{id}.png")))
                    .await?;
                Ok(json!({"artifact_id":id,"mime_type":"image/png","bytes":len}))
            }
            "events" => {
                let events = cdp
                    .events
                    .lock()
                    .expect("event lock")
                    .iter()
                    .filter(|event| {
                        event["sessionId"] == page.session
                            && event["method"].as_str().is_some_and(|m| {
                                m.starts_with("Network.")
                                    || m.starts_with("Runtime.console")
                                    || m == "Runtime.exceptionThrown"
                            })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(json!({"events":events,"capacity":512,"truncated_possible":true}))
            }
            "input" => {
                page.refs.clear();
                host_input(cdp, session, &args).await
            }
            _ => Err(Error::Invalid(format!("unknown action {action}"))),
        }
    }

    async fn launch(&self) -> Result<Running, Error> {
        tokio::fs::create_dir_all(&self.config.data_dir).await?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.config.data_dir.join("profile.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|_| Error::Invalid("browser profile already in use".into()))?;
        self.launch_with_lock(lock).await
    }

    async fn launch_with_lock(&self, lock: File) -> Result<Running, Error> {
        let profile = self.config.data_dir.join("profile");
        tokio::fs::create_dir_all(&profile).await?;
        let port_file = profile.join("DevToolsActivePort");
        match tokio::fs::remove_file(&port_file).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut command = Command::new(&self.config.executable);
        command
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--window-size=1280,720",
            ]);
        if self.config.headless {
            command.arg("--headless=new");
        }
        if self.config.no_sandbox {
            command.arg("--no-sandbox");
        }
        command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (child, group) = self.process_scope.spawn(command)?;
        Ok(Running {
            child,
            group: Some(group),
            cdp: None,
            pages: HashMap::new(),
            recording: None,
            _profile_lock: lock,
        })
    }

    async fn connect(&self, child: &mut Child) -> Result<Arc<Cdp>, Error> {
        let port_file = self.config.data_dir.join("profile/DevToolsActivePort");
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(status) = child.try_wait()? {
                    return Err(Error::Transport(format!(
                        "Chromium exited during launch: {status}"
                    )));
                }
                if let Ok(contents) = tokio::fs::read_to_string(&port_file).await {
                    let mut lines = contents.lines();
                    if let (Some(port), Some(path)) = (lines.next(), lines.next())
                        && port.parse::<u16>().is_ok()
                        && path.starts_with("/devtools/browser/")
                    {
                        let cdp = Arc::new(
                            Cdp::connect(
                                &format!("ws://127.0.0.1:{port}{path}"),
                                self.frames.clone(),
                            )
                            .await?,
                        );
                        *self.connection.write().expect("connection lock") = Some(cdp.clone());
                        return Ok(cdp);
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| Error::Timeout)?
    }

    pub async fn close(&self) -> Result<(), Error> {
        self.closed.store(true, Ordering::Release);
        self.closing.notify_waiters();
        self.connection.write().expect("connection lock").take();
        let mut guard = self.running.lock().await;
        if let Some(running) = guard.as_mut() {
            stop_running(running).await?;
        }
        guard.take();
        Ok(())
    }

    async fn artifact_path(&self, id: &str) -> Result<(PathBuf, &'static str), Error> {
        let id =
            Uuid::parse_str(id).map_err(|_| Error::Invalid("artifact ID must be UUID".into()))?;
        for (extension, mime) in [("png", "image/png"), ("mp4", "video/mp4")] {
            let path = self.config.artifact_dir.join(format!("{id}.{extension}"));
            if tokio::fs::try_exists(&path).await? {
                return Ok((path, mime));
            }
        }
        Err(Error::Invalid("unknown artifact".into()))
    }
}

impl Drop for BrowserService {
    fn drop(&mut self) {
        self.process_scope.kill_all();
    }
}

async fn stop_running(running: &mut Running) -> Result<(), Error> {
    if let Some(recording) = running.recording.as_mut() {
        recording.cancel().await?;
    }
    running.recording.take();
    if let Some(cdp) = running.cdp.as_mut() {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            cdp.call(None, "Browser.close", json!({})),
        )
        .await;
    }
    match tokio::time::timeout(Duration::from_secs(3), running.child.wait()).await {
        Ok(status) => {
            status?;
        }
        Err(_) => {
            if let Some(group) = &running.group {
                group.kill()?;
            }
            running.child.wait().await?;
        }
    }
    running.group.take();
    if let Some(cdp) = running.cdp.as_mut() {
        cdp.stop().await;
    }
    running.cdp.take();
    Ok(())
}

async fn host_input(cdp: &Cdp, session: Option<&str>, args: &Value) -> Result<Value, Error> {
    let kind = string(args, "kind")?;
    let method = match kind {
        "mouse" => "Input.dispatchMouseEvent",
        "key" => "Input.dispatchKeyEvent",
        "text" => "Input.insertText",
        _ => return Err(Error::Unsupported(format!("input {kind}"))),
    };
    let params = args
        .get("params")
        .filter(|v| v.is_object())
        .ok_or_else(|| Error::Invalid("input params object required".into()))?;
    cdp.input_generation.fetch_add(1, Ordering::AcqRel);
    cdp.call(session, method, params.clone()).await
}

async fn page_state(cdp: &Cdp, session: Option<&str>) -> Result<Value, Error> {
    let history = cdp
        .call(session, "Page.getNavigationHistory", json!({}))
        .await?;
    let state = cdp.call(session,"Runtime.evaluate",json!({"expression":"({url:location.href,title:document.title,loading:document.readyState!=='complete'})","returnByValue":true})).await?;
    let mut state = state["result"]["value"].clone();
    if !state.is_object() {
        return Err(Error::Protocol(
            "page state unavailable during navigation".into(),
        ));
    }
    let current = history["currentIndex"].as_u64().unwrap_or(0);
    let count = history["entries"].as_array().map_or(0, Vec::len) as u64;
    state["can_go_back"] = json!(current > 0);
    state["can_go_forward"] = json!(current + 1 < count);
    Ok(state)
}

fn site_origin(value: &str) -> Result<String, Error> {
    let parsed = url::Url::parse(value)
        .map_err(|_| Error::Invalid("valid HTTP(S) origin required".into()))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(Error::Invalid(
            "origin must be HTTP(S) scheme/host/port only".into(),
        ));
    }
    Ok(parsed.origin().ascii_serialization())
}

async fn revision(cdp: &Cdp, session: Option<&str>, context: i64) -> Result<u64, Error> {
    let result = cdp.call(session,"Runtime.evaluate",json!({"contextId":context,"returnByValue":true,"expression":"(()=>{if(!globalThis.__sophonRevision){const s={n:1};new MutationObserver(()=>s.n++).observe(document,{subtree:true,childList:true,attributes:true,characterData:true});globalThis.__sophonRevision=s}return globalThis.__sophonRevision.n})()"})).await?;
    result["result"]["value"].as_u64().ok_or(Error::StaleRef)
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value[key]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("{key} string required")))
}

fn validate_url(url: &str) -> Result<(), Error> {
    if url.starts_with("http://") || url.starts_with("https://") || url == "about:blank" {
        Ok(())
    } else {
        Err(Error::Invalid(
            "navigation accepts http(s) or about:blank only".into(),
        ))
    }
}
