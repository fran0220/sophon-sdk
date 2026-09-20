use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use sophon_browser::{BrowserConfig, BrowserService, Error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(root: &std::path::Path) -> BrowserConfig {
    BrowserConfig {
        executable: PathBuf::from(
            std::env::var("SOPHON_CHROMIUM").unwrap_or_else(|_| "/usr/bin/chromium".into()),
        ),
        data_dir: root.join("identity"),
        artifact_dir: root.join("evidence"),
        headless: true,
        no_sandbox: true,
    }
}

async fn call(browser: &BrowserService, action: &str, mut args: Value) -> Value {
    args["action"] = json!(action);
    let result = if matches!(
        action,
        "evaluate"
            | "artifact"
            | "input"
            | "viewport"
            | "state"
            | "back"
            | "forward"
            | "reload"
            | "clear_site"
            | "clear_profile"
            | "stream_start"
            | "stream_stop"
    ) {
        browser.execute_host(args).await
    } else {
        browser.execute("browser", args).await
    };
    result.unwrap_or_else(|e| panic!("{action}: {e}"))
}

async fn evaluate(browser: &BrowserService, tab: &str, expression: &str) -> Value {
    call(
        browser,
        "evaluate",
        json!({"tab_id":tab,"expression":expression}),
    )
    .await["value"]
        .clone()
}

fn reference(snapshot: &Value, role: &str, name: &str) -> String {
    snapshot["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["role"] == role && n["name"] == name)
        .unwrap_or_else(|| panic!("missing {role} {name}: {snapshot}"))["ref"]
        .as_str()
        .unwrap()
        .into()
}

#[tokio::test]
async fn capabilities_do_not_launch_and_close_is_terminal() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.executable = "missing-chromium".into();
    let browser = BrowserService::new(cfg);
    assert_eq!(
        call(&browser, "capabilities", json!({})).await["audio"],
        false
    );
    assert!(!root.path().join("identity").exists());
    assert!(matches!(
        browser.execute("browser", json!({"action":"audio"})).await,
        Err(Error::Unsupported(_))
    ));
    for action in [
        "clear_profile",
        "clear_site",
        "evaluate",
        "input",
        "artifact",
        "stream_start",
        "viewport",
    ] {
        assert!(
            matches!(
                browser.execute("browser", json!({"action":action})).await,
                Err(Error::Unsupported(_))
            ),
            "host action {action} must not bypass agent schema"
        );
    }
    assert!(!root.path().join("identity").exists());
    assert!(matches!(
        browser
            .execute_host(json!({"action":"artifact","artifact_id":"../../secret"}))
            .await,
        Err(Error::Invalid(_))
    ));
    browser.close().await.unwrap();
    assert!(matches!(
        browser
            .execute("browser", json!({"action":"capabilities"}))
            .await,
        Err(Error::Closed)
    ));
}

/// Requires a real Chromium and ffmpeg/ffprobe. No successful skip when absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires real Chromium and ffmpeg; run with --ignored"]
async fn real_chromium_tools_stream_record_and_cleanup() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let issued = Arc::new(tokio::sync::Notify::new());
    let issued_server = issued.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let issued = issued_server.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let n = socket.read(&mut request).await.unwrap();
                if String::from_utf8_lossy(&request[..n]).starts_with("GET /issued ") {
                    issued.notify_one();
                }
                let child = String::from_utf8_lossy(&request[..n]).starts_with("GET /frame ");
                let body = if child {
                    "<button onclick=\"this.textContent='Child clicked'\">Child button</button>"
                } else {
                    r#"<!doctype html><title>SDK browser fixture</title>
<style>body{font:20px sans-serif;padding:30px}button,input{font:inherit;padding:12px}iframe{display:block;margin:60px;width:400px;height:150px}#animation{width:30px;height:30px;background:red;animation:move .8s infinite alternate}@keyframes move{to{transform:translateX(300px)}}</style>
<h1>Native browser fixture</h1><label>Name <input aria-label="Name"></label>
<button onclick="window.clicks=(window.clicks||0)+1;document.querySelector('#result').textContent='Clicked '+window.clicks;console.log('clicked-once')">Increment</button><p id="result">Ready</p><div id="animation"></div><iframe src="/frame"></iframe>
<script>console.log('fixture-loaded');</script>"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let root = tempfile::tempdir().unwrap();
    let browser = Arc::new(BrowserService::new(config(root.path())));
    let tab = call(&browser, "new_tab", json!({})).await["tab_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let competing = BrowserService::new(config(root.path()));
    assert!(matches!(
        competing.execute("browser", json!({"action":"tabs"})).await,
        Err(Error::Invalid(_))
    ));
    competing.close().await.unwrap();
    call(
        &browser,
        "navigate",
        json!({"tab_id":tab,"url":format!("http://{address}/")}),
    )
    .await;
    for _ in 0..50 {
        if evaluate(&browser, &tab, "document.readyState").await == "complete" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    call(
        &browser,
        "viewport",
        json!({"tab_id":tab,"width":1013,"height":677}),
    )
    .await;
    assert_eq!(
        evaluate(&browser, &tab, "[innerWidth,innerHeight]").await,
        json!([1013, 677])
    );
    let state = call(&browser, "state", json!({"tab_id":tab})).await;
    assert_eq!(state["title"], "SDK browser fixture");
    assert_eq!(state["loading"], false);
    let snap = call(&browser, "snapshot", json!({"tab_id":tab})).await;
    let input = reference(&snap, "textbox", "Name");
    call(
        &browser,
        "type",
        json!({"tab_id":tab,"ref":input,"text":"Native 42"}),
    )
    .await;
    assert_eq!(
        evaluate(&browser, &tab, "document.querySelector('input').value").await,
        "Native 42"
    );
    call(&browser, "key", json!({"tab_id":tab,"key":"Backspace"})).await;
    assert_eq!(
        evaluate(&browser, &tab, "document.querySelector('input').value").await,
        "Native 4"
    );
    call(
        &browser,
        "scroll",
        json!({"tab_id":tab,"delta_y":50,"delta_x":0}),
    )
    .await;
    let snap = call(&browser, "snapshot", json!({"tab_id":tab})).await;
    let old = reference(&snap, "button", "Increment");
    let snap = call(&browser, "snapshot", json!({"tab_id":tab})).await;
    assert!(matches!(
        browser
            .execute("browser", json!({"action":"click","tab_id":tab,"ref":old}))
            .await,
        Err(Error::StaleRef)
    ));
    let button = reference(&snap, "button", "Increment");
    call(&browser, "click", json!({"tab_id":tab,"ref":button})).await;
    assert_eq!(evaluate(&browser, &tab, "window.clicks").await, 1);
    assert!(matches!(
        browser
            .execute(
                "browser",
                json!({"action":"click","tab_id":tab,"ref":button})
            )
            .await,
        Err(Error::StaleRef)
    ));
    // External mutation, not another browser tool, must invalidate the snapshot.
    evaluate(
        &browser,
        &tab,
        "setTimeout(()=>document.querySelector('#result').textContent='External change',300)",
    )
    .await;
    let snap = call(&browser, "snapshot", json!({"tab_id":tab})).await;
    let mutated = reference(&snap, "button", "Increment");
    call(&browser, "wait", json!({"tab_id":tab,"milliseconds":350})).await;
    assert!(matches!(
        browser
            .execute(
                "browser",
                json!({"action":"click","tab_id":tab,"ref":mutated})
            )
            .await,
        Err(Error::StaleRef)
    ));
    let snap = call(&browser, "snapshot", json!({"tab_id":tab})).await;
    let stale = reference(&snap, "button", "Increment");
    let mut frames = browser.subscribe_frames();
    call(&browser, "stream_start", json!({"tab_id":tab})).await;
    let frame = tokio::time::timeout(Duration::from_secs(5), frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame["tab_id"], tab);
    assert!(
        base64::engine::general_purpose::STANDARD
            .decode(frame["base64"].as_str().unwrap())
            .unwrap()
            .starts_with(&[255, 216])
    );
    // Host input is independent of agent tools and invalidates refs.
    call(
        &browser,
        "input",
        json!({"tab_id":tab,"kind":"text","params":{"text":"!"}}),
    )
    .await;
    assert!(matches!(
        browser
            .execute(
                "browser",
                json!({"action":"click","tab_id":tab,"ref":stale})
            )
            .await,
        Err(Error::StaleRef)
    ));
    let mut slow = browser.subscribe_frames();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(matches!(
        slow.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    assert_eq!(evaluate(&browser, &tab, "6*7").await, 42); // Control replies survived frame pressure.
    evaluate(&browser, &tab, "document.querySelector('input').focus()").await;
    let waiting_browser = browser.clone();
    let waiting_tab = tab.clone();
    let waiting = tokio::spawn(async move {
        waiting_browser
            .execute(
                "browser",
                json!({"action":"wait","tab_id":waiting_tab,"milliseconds":10000}),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(
        Duration::from_millis(500),
        browser.execute_host(
            json!({"action":"input","tab_id":tab,"kind":"text","params":{"text":"-live"}}),
        ),
    )
    .await
    .expect("human input must not wait behind agent delay")
    .unwrap();
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    assert!(
        evaluate(&browser, &tab, "document.querySelector('input').value")
            .await
            .as_str()
            .unwrap()
            .ends_with("-live")
    );
    call(&browser, "stream_stop", json!({"tab_id":tab})).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut stopped = browser.subscribe_frames();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stopped.recv())
            .await
            .is_err()
    );
    call(&browser, "stream_start", json!({"tab_id":tab})).await;
    let pending_browser = browser.clone();
    let pending_tab = tab.clone();
    let cancelled = tokio::spawn(async move {
        pending_browser.execute_host(json!({"action":"evaluate","tab_id":pending_tab,"expression":"window.cancelledCount=(window.cancelledCount||0)+1;fetch('/issued');new Promise(r=>setTimeout(()=>r(1),10000))"})).await
    });
    tokio::time::timeout(Duration::from_secs(3), issued.notified())
        .await
        .unwrap();
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    assert_eq!(evaluate(&browser, &tab, "window.cancelledCount").await, 1);
    let tree = call(&browser, "frames", json!({"tab_id":tab})).await;
    let child = tree["frameTree"]["childFrames"][0]["frame"]["id"]
        .as_str()
        .unwrap();
    let child_snap = call(&browser, "snapshot", json!({"tab_id":tab,"frame_id":child})).await;
    let child_button = reference(&child_snap, "button", "Child button");
    call(&browser, "click", json!({"tab_id":tab,"ref":child_button})).await;
    assert_eq!(
        evaluate(
            &browser,
            &tab,
            "document.querySelector('iframe').contentDocument.querySelector('button').textContent"
        )
        .await,
        "Child clicked"
    );
    let image = call(&browser, "screenshot", json!({"tab_id":tab})).await;
    let artifact = call(
        &browser,
        "artifact",
        json!({"artifact_id":image["artifact_id"]}),
    )
    .await;
    assert!(
        base64::engine::general_purpose::STANDARD
            .decode(artifact["base64"].as_str().unwrap())
            .unwrap()
            .starts_with(b"\x89PNG")
    );
    let events = call(&browser, "events", json!({"tab_id":tab})).await;
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["method"] == "Runtime.consoleAPICalled")
    );
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["method"] == "Network.responseReceived")
    );
    assert!(matches!(browser.execute_host(json!({"action":"evaluate","tab_id":tab,"expression":"throw new Error('expected-probe-error')"})).await,Err(Error::Protocol(_))));
    call(&browser, "record_start", json!({"tab_id":tab})).await;
    let started = Instant::now();
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let video = call(&browser, "record_stop", json!({"tab_id":tab})).await;
    let elapsed = started.elapsed().as_secs_f64();
    let video_path = root
        .path()
        .join("evidence")
        .join(format!("{}.mp4", video["artifact_id"].as_str().unwrap()));
    let probe = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&video_path)
        .output()
        .await
        .unwrap();
    assert!(probe.status.success());
    let duration: f64 = String::from_utf8(probe.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        (duration - 1.6).abs() < 0.3,
        "video duration {duration}, elapsed including encoding {elapsed}"
    );
    println!("native recording duration: {duration:.3}s (requested 1.600s)");
    if let Ok(directory) = std::env::var("SOPHON_BROWSER_EVIDENCE_DIR") {
        tokio::fs::create_dir_all(&directory).await.unwrap();
        tokio::fs::copy(
            root.path()
                .join("evidence")
                .join(format!("{}.png", image["artifact_id"].as_str().unwrap())),
            PathBuf::from(&directory).join("native-browser.png"),
        )
        .await
        .unwrap();
        tokio::fs::copy(
            &video_path,
            PathBuf::from(directory).join("native-browser.mp4"),
        )
        .await
        .unwrap();
    }
    // A quiet page still records its held frame for the real elapsed duration.
    evaluate(
        &browser,
        &tab,
        "document.querySelector('#animation').remove()",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    call(&browser, "record_start", json!({"tab_id":tab})).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let quiet = call(&browser, "record_stop", json!({"tab_id":tab})).await;
    assert_eq!(
        call(
            &browser,
            "artifact",
            json!({"artifact_id":quiet["artifact_id"]})
        )
        .await["mime_type"],
        "video/mp4"
    );
    // Persist account identity and artifacts across checked shutdown/relaunch.
    evaluate(
        &browser,
        &tab,
        "localStorage.setItem('identity-test','retained')",
    )
    .await;
    call(&browser, "record_start", json!({"tab_id":tab})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let pending_browser = browser.clone();
    let pending_tab = tab.clone();
    let pending = tokio::spawn(async move {
        pending_browser
            .execute(
                "browser",
                json!({"action":"wait","tab_id":pending_tab,"milliseconds":10000}),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    browser.close().await.unwrap();
    assert!(matches!(pending.await.unwrap(), Err(Error::Closed)));
    assert!(
        std::fs::read_dir(root.path().join("evidence"))
            .unwrap()
            .all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("recording-"))
    );
    let reopened = BrowserService::new(config(root.path()));
    assert_eq!(
        call(
            &reopened,
            "artifact",
            json!({"artifact_id":image["artifact_id"]})
        )
        .await["mime_type"],
        "image/png"
    );
    let tab = call(
        &reopened,
        "new_tab",
        json!({"url":format!("http://{address}/")}),
    )
    .await["tab_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        evaluate(&reopened, &tab, "localStorage.getItem('identity-test')").await,
        "retained"
    );
    call(&reopened, "stream_start", json!({"tab_id":tab})).await;
    call(&reopened, "close_tab", json!({"tab_id":tab})).await;
    assert!(
        reopened
            .execute("browser", json!({"action":"screenshot","tab_id":tab}))
            .await
            .is_err()
    );
    let a = call(
        &reopened,
        "new_tab",
        json!({"url":format!("http://{address}/")}),
    )
    .await["tab_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = call(
        &reopened,
        "new_tab",
        json!({"url":format!("http://localhost:{}/",address.port())}),
    )
    .await["tab_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_millis(200)).await;
    evaluate(
        &reopened,
        &a,
        "setInterval(()=>localStorage.setItem('identity-test','repopulated'),10)",
    )
    .await;
    evaluate(&reopened, &b, "localStorage.setItem('other-origin','keep')").await;
    call(
        &reopened,
        "navigate",
        json!({"tab_id":b,"url":format!("http://localhost:{}/second",address.port())}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let back = call(&reopened, "back", json!({"tab_id":b})).await;
    assert_eq!(back["url"], format!("http://localhost:{}/", address.port()));
    assert_eq!(back["can_go_forward"], true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let forward = call(&reopened, "forward", json!({"tab_id":b})).await;
    assert_eq!(
        forward["url"],
        format!("http://localhost:{}/second", address.port())
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    call(&reopened, "reload", json!({"tab_id":b})).await;
    let clear = call(
        &reopened,
        "clear_site",
        json!({"origin":format!("http://{address}")}),
    )
    .await;
    assert_eq!(clear["closed_all_tabs"], true);
    let a = call(
        &reopened,
        "new_tab",
        json!({"url":format!("http://{address}/")}),
    )
    .await["tab_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = call(
        &reopened,
        "new_tab",
        json!({"url":format!("http://localhost:{}/",address.port())}),
    )
    .await["tab_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        evaluate(&reopened, &a, "localStorage.getItem('identity-test')").await,
        Value::Null
    );
    assert_eq!(
        evaluate(&reopened, &b, "localStorage.getItem('other-origin')").await,
        "keep"
    );
    evaluate(
        &reopened,
        &b,
        "setInterval(()=>localStorage.setItem('other-origin','repopulated'),10)",
    )
    .await;
    call(&reopened, "clear_profile", json!({})).await;
    assert!(!root.path().join("identity/profile").exists());
    let b = call(
        &reopened,
        "new_tab",
        json!({"url":format!("http://localhost:{}/",address.port())}),
    )
    .await["tab_id"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        evaluate(&reopened, &b, "localStorage.getItem('other-origin')").await,
        Value::Null
    );
    assert_eq!(
        call(
            &reopened,
            "artifact",
            json!({"artifact_id":image["artifact_id"]})
        )
        .await["mime_type"],
        "image/png"
    );
    reopened.close().await.unwrap();
    server.abort();
}
