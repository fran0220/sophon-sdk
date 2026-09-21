use base64::Engine;
use serde_json::{Value, json};
use sophon_browser::{BrowserConfig, BrowserService};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn call(browser: &BrowserService, args: Value) -> Value {
    browser.execute("browser", args).await.unwrap()
}

async fn wait_download(browser: &BrowserService, suffix: &str, terminal: bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let list = call(browser, json!({"action":"downloads"})).await;
            if let Some(download) = list["downloads"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["url"].as_str().unwrap().ends_with(suffix))
            {
                let status = call(
                    browser,
                    json!({"action":"download_status","download_id":download["download_id"]}),
                )
                .await;
                if !terminal || matches!(status["state"].as_str(), Some("completed" | "canceled")) {
                    return status;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires real Chromium"]
async fn real_download_artifacts_cancel_size_and_cleanup() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let mut requests = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.unwrap();
                    requests.spawn(async move {
                        let mut buffer = [0u8;4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);
                        let path = request.split_whitespace().nth(1).unwrap_or("/");
                        if path == "/" {
                            let body = "<!doctype html><a href='/small'>Small</a><a href='/slow'>Slow</a><a href='/huge'>Huge</a><a href='/close'>Close</a>";
                            let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await;
                        } else {
                            let length = if path == "/small" { 19 } else if path == "/huge" { 33*1024*1024 } else { 1000000 };
                            if socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"../../untrusted.exe\"\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").as_bytes()).await.is_err() {return}
                            if path == "/small" { let _ = socket.write_all(b"download-evidence73").await; }
                            else {
                                for _ in 0..1000 {
                                    if socket.write_all(&[42;1000]).await.is_err() { break }
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                }
                            }
                        }
                    });
                }
                _ = requests.join_next(), if !requests.is_empty() => {}
            }
        }
    });
    let root = tempfile::tempdir().unwrap();
    let config = BrowserConfig {
        executable: std::env::var_os("SOPHON_CHROMIUM")
            .unwrap_or_else(|| "/usr/bin/chromium".into())
            .into(),
        data_dir: root.path().join("identity"),
        artifact_dir: root.path().join("artifacts"),
        headless: true,
        no_sandbox: true,
        ffmpeg_executable: "ffmpeg".into(),
    };
    let browser = BrowserService::new(config.clone());
    let tab = call(
        &browser,
        json!({"action":"new_tab","url":format!("http://{address}/")}),
    )
    .await["tab_id"]
        .clone();
    for suffix in ["small", "huge", "slow", "close"] {
        call(
            &browser,
            json!({"action":"wait","tab_id":tab,"milliseconds":100}),
        )
        .await;
        let snapshot = call(&browser, json!({"action":"snapshot","tab_id":tab})).await;
        let link = snapshot["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["role"] == "link" && n["name"].as_str().unwrap().to_lowercase() == suffix)
            .unwrap();
        call(
            &browser,
            json!({"action":"click","tab_id":tab,"ref":link["ref"]}),
        )
        .await;
        let status = wait_download(&browser, suffix, suffix == "small" || suffix == "huge").await;
        if suffix == "small" {
            assert_eq!(status["state"], "completed");
            assert_eq!(status["mime_type"], "application/octet-stream");
            assert_eq!(status["bytes"], 19);
            let bytes = browser
                .execute_host(json!({"action":"artifact","artifact_id":status["artifact_id"]}))
                .await
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(bytes["base64"].as_str().unwrap())
                    .unwrap(),
                b"download-evidence73"
            );
            assert!(!root.path().join("untrusted.exe").exists());
        } else if suffix == "huge" {
            assert_eq!(status["state"], "canceled");
            assert_eq!(status["reason"], "size_limit");
            assert!(status.get("artifact_id").is_none());
        } else if suffix == "slow" {
            call(
                &browser,
                json!({"action":"download_cancel","download_id":status["download_id"]}),
            )
            .await;
            let cancelled = wait_download(&browser, suffix, true).await;
            assert_eq!(cancelled["state"], "canceled");
            assert_eq!(cancelled["reason"], "user_cancelled");
            assert!(cancelled.get("artifact_id").is_none());
        }
    }
    browser.close().await.unwrap();
    let entries: Vec<_> = std::fs::read_dir(&config.artifact_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].ends_with(".download"));
    let reopened = BrowserService::new(config);
    let id = entries[0].strip_suffix(".download").unwrap();
    assert!(
        reopened
            .execute_host(json!({"action":"artifact","artifact_id":id}))
            .await
            .is_ok()
    );
    reopened.close().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}
