//! Disposable HTTP contract tests. ffmpeg creates real fixtures and the native
//! service independently decodes responses. These are NOT live model tests.
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sophon_sdk::native_media::{MediaEndpoint, MediaRoute, NativeMediaConfig, NativeMediaService};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

struct Reply {
    status: u16,
    bytes: Vec<u8>,
}
fn json_reply(value: Value) -> Reply {
    Reply {
        status: 200,
        bytes: value.to_string().into_bytes(),
    }
}
fn server(replies: Vec<Reply>) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("HTTP fixture accept: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut b = [0];
                stream.read_exact(&mut b).unwrap();
                request.push(b[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break request.len();
                }
            };
            let headers = String::from_utf8_lossy(&request);
            let len = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|n| n.parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            request.resize(header_end + len, 0);
            stream.read_exact(&mut request[header_end..]).unwrap();
            requests.push(request);
            write!(
                stream,
                "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reply.bytes.len()
            )
            .unwrap();
            stream.write_all(&reply.bytes).unwrap();
        }
        requests
    });
    (base, handle)
}
fn service(base: &str) -> NativeMediaService {
    let route = |endpoint| MediaRoute {
        endpoint,
        model: "explicit-wire-model".into(),
        base_url: base.into(),
        bearer_token: Some("fixture-secret".into()),
        headers: BTreeMap::from([("x-route".into(), "explicit".into())]),
    };
    NativeMediaService::new(
        NativeMediaConfig {
            image: Some(route(MediaEndpoint::ImageGeneration)),
            speech: Some(route(MediaEndpoint::AudioTts)),
            video: Some(route(MediaEndpoint::OpenaiVideo)),
        },
        "ffmpeg".into(),
    )
}
fn fixture(root: &Path, kind: &str) -> Vec<u8> {
    let path = root.join(format!("fixture.{kind}"));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-f", "lavfi", "-i"]);
    if kind == "mp3" {
        cmd.args(["sine=frequency=643:duration=0.1", "-c:a", "libmp3lame"]);
    } else {
        cmd.args([
            "color=c=red:s=32x24:d=0.1",
            "-frames:v",
            "1",
            "-threads",
            "1",
        ]);
    }
    assert!(
        cmd.arg(&path)
            .status()
            .expect("ffmpeg required for media contract tests")
            .success()
    );
    std::fs::read(path).unwrap()
}
fn body(request: &[u8]) -> Value {
    let start = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    serde_json::from_slice(&request[start..]).unwrap()
}

#[tokio::test]
async fn image_json_and_ordered_multipart_use_explicit_route_and_publish_decoded_bytes() {
    let root = tempfile::tempdir().unwrap();
    let png = fixture(root.path(), "png");
    let second_root = tempfile::tempdir().unwrap();
    let second_path = second_root.path().join("second.png");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=24x32:d=0.1",
                "-frames:v",
                "1",
                "-threads",
                "1"
            ])
            .arg(&second_path)
            .status()
            .unwrap()
            .success()
    );
    let second_png = std::fs::read(second_path).unwrap();
    std::fs::write(root.path().join("second.png"), &second_png).unwrap();
    let second_digest = format!("{:x}", Sha256::digest(&second_png));
    let response =
        json!({"data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode(&png)}]});
    let (base, server) = server(vec![json_reply(response.clone()), json_reply(response)]);
    let service = service(&base);
    let result = service
        .execute(
            "generate_image",
            json!({"prompt":"red rectangle","output_path":"first.png","size":"32x24"}),
            root.path(),
        )
        .await
        .unwrap();
    assert_eq!(result["artifact"]["mimeType"], "image/png");
    assert_eq!(result["width"], 32);
    assert_eq!(result["height"], 24);
    assert_eq!(result["requestedSize"], "32x24");
    assert_eq!(
        result["requestedDimensions"],
        json!({"width":32,"height":24})
    );
    assert_eq!(result["dimensionMismatch"], false);
    assert_eq!(result["warnings"], json!([]));
    assert_eq!(std::fs::read(root.path().join("first.png")).unwrap(), png);
    let digest = format!("{:x}", Sha256::digest(&png));
    assert_eq!(result["revision"], digest);
    let edited = service.execute("generate_image",json!({"prompt":"edit rectangle","output_path":"edited.png","size":"24x32","references":[{"path":"second.png","revision":second_digest},{"path":"fixture.png","revision":digest}]}),root.path()).await.unwrap();
    assert_eq!(edited["width"], 32);
    assert_eq!(edited["height"], 24);
    assert_eq!(edited["requestedSize"], "24x32");
    assert_eq!(
        edited["requestedDimensions"],
        json!({"width":24,"height":32})
    );
    assert_eq!(edited["dimensionMismatch"], true);
    assert_eq!(
        edited["warnings"],
        json!([{"code":"image_dimensions_mismatch","requested":{"width":24,"height":32},"actual":{"width":32,"height":24}}])
    );
    assert_eq!(
        std::fs::read(root.path().join("edited.png")).unwrap(),
        png,
        "mismatched paid artifact must remain byte-for-byte intact, not resized/cropped/discarded"
    );
    assert_eq!(edited["revision"], digest);
    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "dimension mismatch must not retry generation"
    );
    let first = String::from_utf8_lossy(&requests[0]);
    assert!(first.starts_with("POST /v1/images/generations "));
    assert!(first.contains("authorization: Bearer fixture-secret"));
    assert!(first.contains("x-route: explicit"));
    assert_eq!(
        body(&requests[0]),
        json!({"model":"explicit-wire-model","prompt":"red rectangle","size":"32x24","n":1,"response_format":"b64_json"})
    );
    let second = String::from_utf8_lossy(&requests[1]);
    assert!(second.starts_with("POST /v1/images/edits "));
    assert!(second.contains("multipart/form-data; boundary="));
    assert!(second.contains("name=\"image[]\"; filename=\"reference.png\""));
    let red_at = requests[1]
        .windows(png.len())
        .position(|bytes| bytes == png)
        .unwrap();
    let blue_at = requests[1]
        .windows(second_png.len())
        .position(|bytes| bytes == second_png)
        .unwrap();
    assert!(
        blue_at < red_at,
        "reference order must not be reversed or sorted by path"
    );
}

#[tokio::test]
async fn speech_requires_real_mp3_and_never_retries_paid_errors() {
    let root = tempfile::tempdir().unwrap();
    let mp3 = fixture(root.path(), "mp3");
    let (base, server) = server(vec![
        Reply {
            status: 200,
            bytes: mp3.clone(),
        },
        Reply {
            status: 200,
            bytes: b"ID3not an actual mp3".to_vec(),
        },
        Reply {
            status: 429,
            bytes: b"fixture-secret".to_vec(),
        },
    ]);
    let service = service(&base);
    let result = service
        .execute(
            "generate_speech",
            json!({"prompt":"hello","voice":"declared-voice","output_path":"speech.mp3"}),
            root.path(),
        )
        .await
        .unwrap();
    assert_eq!(result["artifact"]["mimeType"], "audio/mpeg");
    assert_eq!(std::fs::read(root.path().join("speech.mp3")).unwrap(), mp3);
    for path in ["corrupt.mp3", "limited.mp3"] {
        let error = service
            .execute(
                "generate_speech",
                json!({"prompt":"hello","voice":"declared-voice","output_path":path}),
                root.path(),
            )
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("fixture-secret"));
        assert!(!root.path().join(path).exists());
    }
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(String::from_utf8_lossy(&requests[0]).starts_with("POST /v1/audio/speech "));
    assert_eq!(
        body(&requests[0]),
        json!({"model":"explicit-wire-model","input":"hello","voice":"declared-voice","response_format":"mp3"})
    );
}

#[tokio::test]
async fn video_submit_receipt_resume_and_content_are_openai_jobs_not_imagine() {
    let root = tempfile::tempdir().unwrap();
    let mp4 = fixture(root.path(), "mp4");
    let (base, server) = server(vec![
        json_reply(json!({"id":"job_42","status":"queued"})),
        json_reply(json!({"id":"job_42","status":"completed"})),
        Reply {
            status: 200,
            bytes: mp4.clone(),
        },
    ]);
    let service = service(&base);
    let result=service.execute("generate_video",json!({"prompt":"rectangle moving","output_path":"video.mp4","seconds":"8","size":"640x480"}),root.path()).await.unwrap();
    assert_eq!(result["status"], "queued");
    assert!(result.get("progress").is_none());
    assert!(result.get("artifact").is_none());
    let receipt =
        std::fs::read_to_string(root.path().join(result["receipt"].as_str().unwrap())).unwrap();
    assert!(receipt.contains("job_42"));
    assert!(!receipt.contains("fixture-secret"));
    let result = service
        .execute(
            "generate_video",
            json!({"task_id":"job_42","output_path":"video.mp4"}),
            root.path(),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(std::fs::read(root.path().join("video.mp4")).unwrap(), mp4);
    let requests = server.join().unwrap();
    for (request, start) in requests.iter().zip([
        "POST /v1/videos ",
        "GET /v1/videos/job_42 ",
        "GET /v1/videos/job_42/content ",
    ]) {
        assert!(String::from_utf8_lossy(request).starts_with(start));
    }
    assert_eq!(
        body(&requests[0]),
        json!({"model":"explicit-wire-model","prompt":"rectangle moving","seconds":"8","size":"640x480"})
    );
}

#[tokio::test]
async fn reject_unknown_endpoints_paths_changed_references_and_imagine_payload() {
    assert!(
        serde_json::from_value::<NativeMediaConfig>(
            json!({"speech":{"endpoint":"audio","model":"x","baseUrl":"http://localhost/v1"}})
        )
        .is_err()
    );
    let root = tempfile::tempdir().unwrap();
    let (base, server) = server(vec![json_reply(
        json!({"request_id":"imagine_1","status":"done","video":{"url":"https://example.invalid/secret"}}),
    )]);
    let service = service(&base);
    for path in ["../bad.png", "/tmp/bad.png", "nested/../../bad.png"] {
        assert!(
            service
                .execute(
                    "generate_image",
                    json!({"prompt":"x","output_path":path}),
                    root.path()
                )
                .await
                .is_err()
        );
    }
    fixture(root.path(), "png");
    assert!(service.execute("generate_image",json!({"prompt":"x","output_path":"bad.png","references":[{"path":"fixture.png","revision":"incorrect"}]}),root.path()).await.is_err());
    assert!(
        service
            .execute(
                "generate_video",
                json!({"prompt":"x","output_path":"bad.mp4"}),
                root.path()
            )
            .await
            .is_err()
    );
    assert!(!root.path().join("bad.mp4").exists());
    let receipts = std::fs::read_dir(root.path().join(".native-media"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert!(
        std::fs::read_to_string(receipts[0].path())
            .unwrap()
            .contains("outcome_unknown")
    );
    assert_eq!(server.join().unwrap().len(), 1);
}

#[tokio::test]
async fn video_polling_is_bounded_and_cancellation_retains_remote_job() {
    let root = tempfile::tempdir().unwrap();
    let (base, server) = server(vec![
        json_reply(json!({"id":"job_bound","status":"in_progress","progress":17})),
        json_reply(json!({"id":"job_cancel","status":"queued"})),
    ]);
    let service = service(&base);
    let started = Instant::now();
    let result = service
        .execute(
            "generate_video",
            json!({"prompt":"x","output_path":"bound.mp4","poll_seconds":1}),
            root.path(),
        )
        .await
        .unwrap();
    assert_eq!(result["progress"], 17.0);
    assert!(started.elapsed() < Duration::from_secs(3));
    let call = service.execute(
        "generate_video",
        json!({"prompt":"x","output_path":"cancel.mp4","poll_seconds":120}),
        root.path(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), call)
            .await
            .is_err()
    );
    let receipts = std::fs::read_dir(root.path().join(".native-media"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert!(receipts.iter().any(|v| v.contains("job_cancel")));
    assert!(!root.path().join("cancel.mp4").exists());
    assert_eq!(server.join().unwrap().len(), 2);
}

#[tokio::test]
async fn video_rejects_mismatched_job_and_corrupt_completed_content_without_losing_id() {
    let root = tempfile::tempdir().unwrap();
    let (base, server) = server(vec![
        json_reply(json!({"id":"wrong_job","status":"completed"})),
        json_reply(json!({"id":"correct_job","status":"completed"})),
        Reply {
            status: 200,
            bytes: b"\0\0\0\x18ftypisomthis-is-not-a-video".to_vec(),
        },
    ]);
    let service = service(&base);
    for _ in 0..2 {
        assert!(
            service
                .execute(
                    "generate_video",
                    json!({"task_id":"correct_job","output_path":"corrupt.mp4"}),
                    root.path()
                )
                .await
                .is_err()
        );
    }
    assert!(!root.path().join("corrupt.mp4").exists());
    let receipts = std::fs::read_dir(root.path().join(".native-media"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0].contains("correct_job"));
    assert!(!receipts[0].contains("wrong_job"));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.starts_with(b"GET ")));
}

#[tokio::test]
async fn cancellation_during_submission_leaves_uncertainty_receipt_and_no_duplicate_post() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let service = service(&base);
    let call = service.execute(
        "generate_video",
        json!({"prompt":"paid request","output_path":"unknown.mp4"}),
        root.path(),
    );
    let mut call = Box::pin(call);
    let (mut socket, _) = tokio::select! {
        accepted = listener.accept() => accepted.unwrap(),
        result = &mut call => panic!("submission returned before HTTP accept: {result:?}"),
    };
    use tokio::io::AsyncReadExt;
    let mut bytes = [0; 4096];
    let received = tokio::select! {
        bytes = socket.read(&mut bytes) => bytes.unwrap(),
        result = &mut call => panic!("submission returned before HTTP request: {result:?}"),
    };
    assert!(bytes[..received].starts_with(b"POST /v1/videos "));
    // No response: dropping the in-flight execution must leave an uncertainty
    // receipt, and cannot issue a second paid request.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), call)
            .await
            .is_err()
    );
    let receipts = std::fs::read_dir(root.path().join(".native-media"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(receipts.len(), 1);
    let value: Value = serde_json::from_slice(&std::fs::read(receipts[0].path()).unwrap()).unwrap();
    assert_eq!(value["status"], "outcome_unknown");
    assert!(value.get("task_id").is_none());
    assert!(!root.path().join("unknown.mp4").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_symlinks_existing_outputs_and_endpoint_mismatch_fail_before_submit() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    std::fs::write(root.path().join("existing.png"), b"preserve").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let service = service(&base);
    for path in ["escape/out.png", "existing.png"] {
        assert!(
            service
                .execute(
                    "generate_image",
                    json!({"prompt":"x","output_path":path}),
                    root.path()
                )
                .await
                .is_err()
        );
    }
    assert_eq!(
        std::fs::read(root.path().join("existing.png")).unwrap(),
        b"preserve"
    );
    assert!(!outside.path().join("out.png").exists());
    let wrong = NativeMediaService::new(
        NativeMediaConfig {
            image: Some(MediaRoute {
                endpoint: MediaEndpoint::OpenaiVideo,
                model: "declared".into(),
                base_url: base,
                bearer_token: None,
                headers: BTreeMap::new(),
            }),
            ..Default::default()
        },
        "ffmpeg".into(),
    );
    assert!(
        wrong
            .execute(
                "generate_image",
                json!({"prompt":"x","output_path":"out.png"}),
                root.path()
            )
            .await
            .is_err()
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn malformed_status_preserves_job_id_without_persisting_provider_payload() {
    let root = tempfile::tempdir().unwrap();
    let (base, server) = server(vec![json_reply(
        json!({"id":"recoverable_job","status":{"error":"fixture-secret"}}),
    )]);
    let error = service(&base)
        .execute(
            "generate_video",
            json!({"prompt":"x","output_path":"bad.mp4"}),
            root.path(),
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("fixture-secret"));
    let receipts = std::fs::read_dir(root.path().join(".native-media"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(receipts.len(), 1);
    let text = std::fs::read_to_string(receipts[0].path()).unwrap();
    assert!(!text.contains("fixture-secret"));
    let receipt: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(receipt["task_id"], "recoverable_job");
    assert_eq!(receipt["status"], "unrecognized");
    assert_eq!(server.join().unwrap().len(), 1);
}

#[tokio::test]
async fn image_rejects_remote_url_and_corrupt_png_instead_of_publishing_evidence() {
    let root = tempfile::tempdir().unwrap();
    let (base, server) = server(vec![
        json_reply(json!({"data":[{"url":"http://127.0.0.1/private"}]})),
        json_reply(
            json!({"data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nnot an image")}]}),
        ),
    ]);
    for path in ["url.png", "corrupt.png"] {
        assert!(
            service(&base)
                .execute(
                    "generate_image",
                    json!({"prompt":"x","output_path":path}),
                    root.path()
                )
                .await
                .is_err()
        );
        assert!(!root.path().join(path).exists());
    }
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with(b"POST /v1/images/generations "))
    );
}

#[tokio::test]
async fn polling_reuses_the_submitted_job_and_decodes_completed_content() {
    let root = tempfile::tempdir().unwrap();
    let mp4 = fixture(root.path(), "mp4");
    let (base, server) = server(vec![
        json_reply(json!({"id":"polled_job","status":"queued"})),
        json_reply(json!({"id":"polled_job","status":"completed"})),
        Reply {
            status: 200,
            bytes: mp4.clone(),
        },
    ]);
    let result = service(&base)
        .execute(
            "generate_video",
            json!({"prompt":"x","output_path":"polled.mp4","poll_seconds":3}),
            root.path(),
        )
        .await
        .unwrap();
    assert_eq!(result["task_id"], "polled_job");
    assert_eq!(result["status"], "completed");
    assert_eq!(std::fs::read(root.path().join("polled.mp4")).unwrap(), mp4);
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with(b"POST /v1/videos "));
    assert!(requests[1].starts_with(b"GET /v1/videos/polled_job "));
    assert!(requests[2].starts_with(b"GET /v1/videos/polled_job/content "));
}

#[tokio::test]
async fn image_dimensions_come_from_decoded_png_not_provider_metadata_or_requested_size() {
    let root = tempfile::tempdir().unwrap();
    let png = fixture(root.path(), "png");
    let reply = json!({"data":[{"width":999,"height":888,"b64_json":base64::engine::general_purpose::STANDARD.encode(&png)}]});
    let (base, server) = server(vec![
        json_reply(reply.clone()),
        json_reply(reply.clone()),
        json_reply(reply),
    ]);
    for (index, size) in [None, Some("auto"), Some("32x32")].into_iter().enumerate() {
        let path = format!("dimensions-{index}.png");
        let mut args = json!({"prompt":"x","output_path":path});
        if let Some(size) = size {
            args["size"] = json!(size);
        }
        let result = service(&base)
            .execute("generate_image", args, root.path())
            .await
            .unwrap();
        assert_eq!(result["width"], 32);
        assert_eq!(result["height"], 24);
        assert_eq!(result["requestedSize"], json!(size));
        assert_eq!(result["dimensionMismatch"], index == 2);
        if index == 2 {
            assert_eq!(
                result["requestedDimensions"],
                json!({"width":32,"height":32})
            );
            assert_eq!(
                result["warnings"],
                json!([{"code":"image_dimensions_mismatch","requested":{"width":32,"height":32},"actual":{"width":32,"height":24}}])
            );
        } else {
            assert!(result["requestedDimensions"].is_null());
            assert_eq!(result["warnings"], json!([]));
        }
        assert_eq!(std::fs::read(root.path().join(path)).unwrap(), png);
    }
    assert_eq!(server.join().unwrap().len(), 3);
}
