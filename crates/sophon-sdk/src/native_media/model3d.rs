//! Owner-scoped Meshy preview protocol. No result URL fetching or create retries.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    prompt: Option<String>,
    task_id: Option<String>,
    output_path: String,
    #[serde(default)]
    poll_seconds: u32,
}

impl NativeMediaService {
    pub(super) async fn model3d(
        &self,
        route: &MediaRoute,
        client: &Client,
        args: Args,
        workspace: &Path,
    ) -> Result<Value, Error> {
        if args.prompt.is_some() == args.task_id.is_some() || args.poll_seconds > 120 {
            return Err(fail(
                "Model requires prompt OR task_id and poll_seconds 0..120",
            ));
        }
        if let Some(prompt) = &args.prompt {
            nonempty(prompt)?;
            if prompt.chars().count() > 600 {
                return Err(fail("Model prompt exceeds 600 characters"));
            }
        }
        if let Some(id) = &args.task_id {
            valid_id(id)?;
        }
        let output = output_path(workspace, &args.output_path, "glb")?;
        let root = dunce::canonicalize(workspace).map_err(|_| fail("Cannot resolve workspace"))?;
        let receipts = root.join(".native-media");
        std::fs::create_dir_all(&receipts).map_err(|_| fail("Cannot create media receipts"))?;
        let receipts =
            dunce::canonicalize(receipts).map_err(|_| fail("Cannot resolve media receipts"))?;
        if !receipts.starts_with(&root) {
            return Err(fail("Media receipts escape workspace"));
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let receipt = receipts.join(format!("{request_id}.json"));
        let mut result = json!({"request_id":request_id,"receipt":format!(".native-media/{request_id}.json"),"output_path":args.output_path,"kind":"model3d","quality":"preview","status":"outcome_unknown","remoteCancellationSupported":false});
        // Persist before the single paid create. A dropped future never implies refund.
        write_receipt(&receipt, &result)?;
        let id = if let Some(id) = args.task_id {
            id
        } else {
            let response = route.request(client, Method::POST, "meshy/openapi/v2/text-to-3d")?
                .header("Idempotency-Key", &request_id)
                .json(&json!({"mode":"preview","prompt":args.prompt.unwrap(),"ai_model":"latest","should_remesh":false,"target_formats":["glb"]}))
                .send().await.map_err(|_| fail("Model submission outcome unknown; inspect receipts, do not recreate"))?;
            if response.status() != reqwest::StatusCode::ACCEPTED {
                return Err(fail(
                    "Model submission not accepted; inspect receipts, do not recreate",
                ));
            }
            let payload = parse_response(response).await?;
            let id = payload["result"]
                .as_str()
                .ok_or_else(|| fail("Model task ID missing; submission outcome unknown"))?;
            valid_id(id)?;
            id.to_owned()
        };
        result["task_id"] = json!(id);
        result["status"] = json!("pending");
        write_receipt(&receipt, &result)?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(args.poll_seconds));
        // Even zero polling performs one owner-scoped status/content check; all
        // subsequent requests share the caller's remaining polling budget.
        let mut first = true;
        loop {
            let operation = async {
                let response = route
                    .request(client, Method::GET, &format!("meshy/tasks/{id}"))?
                    .send()
                    .await
                    .map_err(|_| fail("Model status unavailable; resume persisted task_id"))?;
                let payload = parse_response(response).await?;
                if payload["task_id"].as_str() != Some(id.as_str())
                    || payload["platform"] != "meshy"
                    || payload["action"] != "text-to-3d"
                {
                    return Err(fail("Model status identity mismatch"));
                }
                match payload["status"].as_str() {
                    Some("FAILURE") => Ok(("failed", None)),
                    Some("SUCCESS") => {
                        let response = route
                            .request(client, Method::GET, &format!("meshy/tasks/{id}/content"))?
                            .send()
                            .await
                            .map_err(|_| {
                                fail("Model content unavailable; resume persisted task_id")
                            })?;
                        if response.status() == reqwest::StatusCode::CONFLICT {
                            return Ok(("content_pending", None));
                        }
                        let bytes = response_bytes(response, 32 * 1024 * 1024).await?;
                        Ok(("downloaded", Some(bytes)))
                    }
                    Some("IN_PROGRESS" | "QUEUED" | "SUBMITTED" | "NOT_START") => {
                        Ok(("pending", None))
                    }
                    _ => Err(fail("Unknown model task status; resume persisted task_id")),
                }
            };
            let (status, bytes) = if first {
                first = false;
                operation.await?
            } else {
                match tokio::time::timeout_at(deadline, operation).await {
                    Ok(value) => value?,
                    Err(_) => return Ok(result),
                }
            };
            result["status"] = json!(status);
            write_receipt(&receipt, &result)?;
            if let Some(bytes) = bytes {
                let geometry = glb::validate(&self.ffmpeg_executable, &bytes).await?;
                let published = publish(&output, &args.output_path, &bytes, "model/gltf-binary")?;
                result["artifact"] = published["artifact"].clone();
                result["geometry"] = geometry;
                result["reviewRequired"] = json!(true);
                result["status"] = json!("completed");
                write_receipt(&receipt, &result)?;
                return Ok(result);
            }
            if status == "failed" || tokio::time::Instant::now() >= deadline {
                return Ok(result);
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                tokio::time::Instant::now() + Duration::from_secs(1),
            ))
            .await;
            if tokio::time::Instant::now() >= deadline {
                return Ok(result);
            }
        }
    }
}
