//! Runtime-local, Electron-independent browser execution. No CLI, MCP or agent loop.
mod cdp;

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use cdp::Cdp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub executable: PathBuf,
    /// Dedicated account/Runtime directory, shared across Games. Never delete on
    /// Game removal and never point at a user's default Chrome profile.
    pub data_dir: PathBuf,
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
}
struct Page {
    session: String,
    refs: HashMap<String, ElementRef>,
}
struct Running {
    child: Child,
    cdp: Option<Cdp>,
    pages: HashMap<String, Page>,
    _profile_lock: File,
}

/// Operations are serialized; dropping a call stops waiting, never retries it.
/// Call `close().await` for checked process cleanup; Drop is only a kill fallback.
pub struct BrowserService {
    config: BrowserConfig,
    running: Mutex<Option<Running>>,
    closed: AtomicBool,
    closing: Notify,
    artifacts: Mutex<HashMap<String, Vec<u8>>>,
}

impl BrowserService {
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            config,
            running: Mutex::new(None),
            closed: AtomicBool::new(false),
            closing: Notify::new(),
            artifacts: Mutex::new(HashMap::new()),
        }
    }

    pub fn tool_specs() -> Vec<BrowserToolSpec> {
        vec![BrowserToolSpec {
            name: "browser".into(),
            description: "Use the Runtime-owned browser. Snapshot gives revision-scoped refs; refresh after page changes. Never retry a timed-out interaction automatically. Screenshots return artifact IDs; audio/video/streaming are unsupported.".into(),
            input_schema: json!({"type":"object","required":["action"],"properties":{
                "action":{"type":"string","enum":["capabilities","tabs","new_tab","close_tab","navigate","frames","snapshot","click","type","key","scroll","wait","screenshot","events"]},
                "tab_id":{"type":"string"},"frame_id":{"type":"string"},"url":{"type":"string"},"ref":{"type":"string"},"text":{"type":"string"},"key":{"type":"string"},"delta_x":{"type":"number"},"delta_y":{"type":"number"},"milliseconds":{"type":"integer","minimum":0,"maximum":10000}
            },"additionalProperties":false}),
        }]
    }

    pub async fn execute(&self, name: &str, args: Value) -> Result<Value, Error> {
        if name != "browser" {
            return Err(Error::Invalid(format!("unknown tool {name}")));
        }
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
                json!({"engine":"chromium-cdp","persistent_profile":true,"semantic_snapshots":true,"frame_snapshots":true,"screenshots":true,"input":true,"console":true,"network_metadata":true,"streaming":false,"audio":false,"recording":false,"cross_origin_frame_snapshots":false}),
            );
        }
        if matches!(action, "stream" | "audio" | "record" | "recording") {
            return Err(Error::Unsupported(action.into()));
        }
        if action == "artifact" {
            let artifacts = self.artifacts.lock().await;
            let bytes = artifacts
                .get(string(&args, "artifact_id")?)
                .ok_or_else(|| Error::Invalid("unknown artifact".into()))?;
            return Ok(
                json!({"mime_type":"image/png","base64":base64::engine::general_purpose::STANDARD.encode(bytes)}),
            );
        }
        if action == "release_artifact" {
            let removed = self
                .artifacts
                .lock()
                .await
                .remove(string(&args, "artifact_id")?)
                .is_some();
            return Ok(json!({"released":removed}));
        }
        let mut guard = self.running.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if guard.is_none() {
            *guard = Some(self.launch().await?);
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
                let result = connection.call(None,"Target.closeTarget",json!({"targetId":tab})).await?;
                running.pages.remove(tab);
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
                },
            );
        }
        let page = running.pages.get_mut(tab).expect("attached");
        let cdp = connection;
        let session = Some(page.session.as_str());
        match action {
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
                            },
                        );
                        item["ref"] = json!(id);
                    }
                    nodes.push(item);
                }
                if revision != self::revision(cdp, session, context).await? {
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
                if revision(cdp, session, reference.context)
                    .await
                    .map_err(|_| Error::StaleRef)?
                    != reference.revision
                {
                    return Err(Error::StaleRef);
                }
                let node = cdp.call(session,"DOM.resolveNode",json!({"backendNodeId":reference.backend,"executionContextId":reference.context})).await.map_err(|_| Error::StaleRef)?;
                let object = string(&node["object"], "objectId")?;
                let function = if action == "click" {
                    "function(){if(!this.isConnected)throw Error('detached');this.scrollIntoView({block:'center',inline:'center'});const r=this.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2,width:r.width,height:r.height}}"
                } else {
                    "function(){if(!this.isConnected)throw Error('detached');this.focus();return document.activeElement===this}"
                };
                let located = cdp.call(session,"Runtime.callFunctionOn",json!({"objectId":object,"functionDeclaration":function,"returnByValue":true})).await?;
                let _ = cdp
                    .call(session, "Runtime.releaseObject", json!({"objectId":object}))
                    .await;
                if located.get("exceptionDetails").is_some() {
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
                    let point = &located["result"]["value"];
                    if point["width"].as_f64().unwrap_or(0.0) <= 0.0
                        || point["height"].as_f64().unwrap_or(0.0) <= 0.0
                    {
                        return Err(Error::Invalid("element has no clickable box".into()));
                    }
                    for kind in ["mousePressed", "mouseReleased"] {
                        cdp.call(session,"Input.dispatchMouseEvent",json!({"type":kind,"x":point["x"],"y":point["y"],"button":"left","clickCount":1})).await?;
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
                let mut artifacts = self.artifacts.lock().await;
                if artifacts.len() >= 32 {
                    return Err(Error::Invalid(
                        "artifact capacity reached; release artifacts before capturing more".into(),
                    ));
                }
                artifacts.insert(id.clone(), bytes);
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
                let kind = string(&args, "kind")?;
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
                cdp.call(session, method, params.clone()).await
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
        let child = command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Running {
            child,
            cdp: None,
            pages: HashMap::new(),
            _profile_lock: lock,
        })
    }

    async fn connect(&self, child: &mut Child) -> Result<Cdp, Error> {
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
                    if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                        if port.parse::<u16>().is_ok() && path.starts_with("/devtools/browser/") {
                            return Cdp::connect(&format!("ws://127.0.0.1:{port}{path}")).await;
                        }
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
        let mut guard = self.running.lock().await;
        if let Some(running) = guard.as_mut() {
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
                    running.child.kill().await?;
                    running.child.wait().await?;
                }
            }
            if let Some(cdp) = running.cdp.as_mut() {
                cdp.stop().await;
            }
        }
        guard.take();
        self.artifacts.lock().await.clear();
        Ok(())
    }
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
