//! Explicit gateway media protocols. No inference, ambient credentials, retries,
//! redirects, or remote-URL downloads. Outputs are decoded before publication.
//!
//! Uses the Runtime's selected FFmpeg executable. Output parents must already exist inside the
//! workspace; existing files are never overwritten. Dropping execution kills
//! decoding and stops polling, but does NOT cancel a remote paid job. Video
//! receipts in `.native-media` survive cancellation; an `outcome_unknown`
//! receipt means submission may have succeeded and must not be blindly retried.
//!
//! Configure each route with the provider's exact model and a versioned base URL
//! (for example a gateway URL ending in `/v1`). `image-generation` uses
//! `/images/generations` or ordered multipart `/images/edits`; `audio-tts` uses
//! `/audio/speech`; `openai-video` uses `/videos`, `/videos/{id}`, and
//! `/videos/{id}/content`. The Imagine `/videos/generations` protocol is not
//! supported. Unknown endpoint declarations fail deserialization.
//!
//! Tool arguments use snake_case; configuration uses camelCase. Image references
//! carry `{path, revision}` with a lowercase SHA-256 digest. Returned artifacts
//! contain `{path, mimeType, bytes, revision}` with a workspace-relative path;
//! `reviewRequired` is not a claim of visual or semantic quality. PNG, MP3 and MP4
//! must contain decoded frames/samples, not merely a matching content type.
//! Image results include actual `width`/`height`, `requestedSize` and parsed
//! `requestedDimensions` (null for omitted/automatic sizes), `dimensionMismatch`,
//! and structured `warnings`. A dimension mismatch preserves the original paid
//! artifact unchanged and never claims that the requested size was satisfied.
//!
//! HTTP operations are bounded to 120 seconds and decoding to 60 seconds.
//! `generate_video.poll_seconds` bounds only subsequent polling (0 by default,
//! maximum 120), not submission or downloading completed content. Resume using
//! `task_id` instead of `prompt`; this service cannot automatically recover a job
//! when cancellation happened before its ID was received. Request bodies,
//! provider errors, credentials, and remote URLs are never included in receipts.

use crate::{Error, protocol::ToolSpec};
use base64::Engine;
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use ts_rs::TS;

#[derive(Clone, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeMediaConfig {
    pub image: Option<MediaRoute>,
    pub speech: Option<MediaRoute>,
    pub video: Option<MediaRoute>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaRoute {
    pub endpoint: MediaEndpoint,
    pub model: String,
    pub base_url: String,
    pub bearer_token: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum MediaEndpoint {
    #[serde(rename = "image-generation")]
    ImageGeneration,
    #[serde(rename = "audio-tts")]
    AudioTts,
    #[serde(rename = "openai-video")]
    OpenaiVideo,
}

pub struct NativeMediaService {
    config: NativeMediaConfig,
    ffmpeg_executable: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    path: String,
    revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageArgs {
    prompt: String,
    output_path: String,
    size: Option<String>,
    #[serde(default)]
    references: Vec<Reference>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
struct ImageDimensions {
    width: u32,
    height: u32,
}

#[derive(Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
enum ImageWarning {
    ImageDimensionsMismatch {
        requested: ImageDimensions,
        actual: ImageDimensions,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeechArgs {
    prompt: String,
    voice: String,
    output_path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VideoArgs {
    prompt: Option<String>,
    task_id: Option<String>,
    output_path: String,
    seconds: Option<String>,
    size: Option<String>,
    #[serde(default)]
    poll_seconds: u32,
}

fn fail(message: &str) -> Error {
    Error::Operation(message.into())
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|_| fail("Invalid media tool arguments"))
}
fn nonempty(value: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        Err(fail("Media input must not be empty"))
    } else {
        Ok(())
    }
}
fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl NativeMediaService {
    pub fn new(config: NativeMediaConfig, ffmpeg_executable: PathBuf) -> Self {
        Self {
            config,
            ffmpeg_executable,
        }
    }

    pub fn tool_specs() -> Vec<ToolSpec> {
        let string = json!({"type":"string", "minLength":1});
        [
            ("generate_image", "Generate a decoded PNG using the explicit image route; references require SHA-256 revisions. Inspect returned width, height and dimensionMismatch: providers may ignore requested size. Preserve mismatched artifacts and report warnings; never claim exact-size success or retry automatically.", json!({
                "prompt":string,"output_path":string,"size":string,
                "references":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["path","revision"],"properties":{"path":string,"revision":string}}}
            }), vec!["prompt","output_path"]),
            ("generate_speech", "Generate and decode MP3 speech using the explicit audio-tts route.", json!({"prompt":string,"voice":string,"output_path":string}), vec!["prompt","voice","output_path"]),
            ("generate_video", "Submit exactly once or resume an OpenAI video task by task_id. Poll at most 120 seconds. Local cancellation does not cancel the remote task; inspect .native-media receipts before retrying an uncertain submission.", json!({"prompt":string,"task_id":string,"output_path":string,"seconds":{"enum":["4","8","12"]},"size":string,"poll_seconds":{"type":"integer","minimum":0,"maximum":120}}), vec!["output_path"]),
        ].into_iter().map(|(name,description,properties,required)| ToolSpec {
            name:name.into(), description:description.into(),
            input_schema:json!({"type":"object","additionalProperties":false,"properties":properties,"required":required}),
        }).collect()
    }

    pub async fn execute(&self, name: &str, args: Value, workspace: &Path) -> Result<Value, Error> {
        let (route, endpoint) = match name {
            "generate_image" => (&self.config.image, MediaEndpoint::ImageGeneration),
            "generate_speech" => (&self.config.speech, MediaEndpoint::AudioTts),
            "generate_video" => (&self.config.video, MediaEndpoint::OpenaiVideo),
            _ => return Err(fail("Unknown native media tool")),
        };
        let route = route
            .as_ref()
            .ok_or_else(|| fail("Media route is not configured"))?;
        route.validate(endpoint)?;
        decoder_available(&self.ffmpeg_executable).await?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| fail("Cannot create media HTTP client"))?;
        match endpoint {
            MediaEndpoint::ImageGeneration => {
                self.image(route, &client, parse(args)?, workspace).await
            }
            MediaEndpoint::AudioTts => self.speech(route, &client, parse(args)?, workspace).await,
            MediaEndpoint::OpenaiVideo => self.video(route, &client, parse(args)?, workspace).await,
        }
    }

    async fn image(
        &self,
        route: &MediaRoute,
        client: &Client,
        args: ImageArgs,
        workspace: &Path,
    ) -> Result<Value, Error> {
        nonempty(&args.prompt)?;
        let output = output_path(workspace, &args.output_path, "png")?;
        let requested_size = args.size.clone();
        let request = if args.references.is_empty() {
            let mut body = json!({"model":route.model,"prompt":args.prompt,"n":1,"response_format":"b64_json"});
            if let Some(size) = args.size {
                body["size"] = json!(size);
            }
            route
                .request(client, Method::POST, "images/generations")?
                .json(&body)
        } else {
            if args.references.len() > 16 {
                return Err(fail("At most 16 image references are supported"));
            }
            let mut form = reqwest::multipart::Form::new()
                .text("model", route.model.clone())
                .text("prompt", args.prompt)
                .text("n", "1")
                .text("response_format", "b64_json");
            if let Some(size) = args.size {
                form = form.text("size", size);
            }
            for reference in args.references {
                let path = input_path(workspace, &reference.path)?;
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .and_then(|file| file.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes))
                    .map_err(|_| fail("Cannot read reference image"))?;
                if bytes.len() > 32 * 1024 * 1024 || revision(&bytes) != reference.revision {
                    return Err(fail("Image reference exceeds limit or revision changed"));
                }
                decode(&self.ffmpeg_executable, &bytes, "png").await?;
                let part = reqwest::multipart::Part::bytes(bytes)
                    .file_name("reference.png")
                    .mime_str("image/png")
                    .map_err(|_| fail("Cannot encode image reference"))?;
                form = form.part("image[]", part);
            }
            route
                .request(client, Method::POST, "images/edits")?
                .multipart(form)
        };
        let response = request
            .send()
            .await
            .map_err(|_| fail("Image submission outcome unknown; not retried"))?;
        let payload: Value =
            serde_json::from_slice(&response_bytes(response, 48 * 1024 * 1024).await?)
                .map_err(|_| fail("Invalid image response JSON"))?;
        let data = payload["data"]
            .as_array()
            .filter(|v| v.len() == 1)
            .ok_or_else(|| fail("Expected exactly one inline image"))?;
        let encoded = data[0]["b64_json"]
            .as_str()
            .ok_or_else(|| fail("Image response requires inline base64 bytes"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| fail("Invalid image base64"))?;
        decode(&self.ffmpeg_executable, &bytes, "png").await?;
        // PNG's mandatory first chunk is IHDR. Read its dimensions only after
        // the complete image has passed the actual decoder, not from API claims.
        let header = bytes
            .get(8..24)
            .filter(|header| &header[..8] == b"\0\0\0\rIHDR")
            .ok_or_else(|| fail("Decoded PNG has no valid IHDR dimensions"))?;
        let actual = ImageDimensions {
            width: u32::from_be_bytes(header[8..12].try_into().unwrap()),
            height: u32::from_be_bytes(header[12..16].try_into().unwrap()),
        };
        let requested = requested_size.as_deref().and_then(|size| {
            let (width, height) = size.split_once('x')?;
            Some(ImageDimensions {
                width: width.parse().ok()?,
                height: height.parse().ok()?,
            })
        });
        let warnings: Vec<ImageWarning> = requested
            .filter(|dimensions| *dimensions != actual)
            .map(|requested| ImageWarning::ImageDimensionsMismatch { requested, actual })
            .into_iter()
            .collect();
        let mut result = publish(&output, &args.output_path, &bytes, "image/png")?;
        result.as_object_mut().unwrap().extend(
            json!({
                "width":actual.width,"height":actual.height,
                "requestedSize":requested_size,"requestedDimensions":requested,
                "dimensionMismatch":!warnings.is_empty(),"warnings":warnings,
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        Ok(result)
    }

    async fn speech(
        &self,
        route: &MediaRoute,
        client: &Client,
        args: SpeechArgs,
        workspace: &Path,
    ) -> Result<Value, Error> {
        nonempty(&args.prompt)?;
        nonempty(&args.voice)?;
        let output = output_path(workspace, &args.output_path, "mp3")?;
        let response = route
            .request(client, Method::POST, "audio/speech")?
            .json(&json!({
                "model":route.model,"input":args.prompt,"voice":args.voice,"response_format":"mp3"
            }))
            .send()
            .await
            .map_err(|_| fail("Speech submission outcome unknown; not retried"))?;
        let bytes = response_bytes(response, 64 * 1024 * 1024).await?;
        decode(&self.ffmpeg_executable, &bytes, "mp3").await?;
        publish(&output, &args.output_path, &bytes, "audio/mpeg")
    }

    async fn video(
        &self,
        route: &MediaRoute,
        client: &Client,
        args: VideoArgs,
        workspace: &Path,
    ) -> Result<Value, Error> {
        let output = output_path(workspace, &args.output_path, "mp4")?;
        if args.prompt.is_some() == args.task_id.is_some() || args.poll_seconds > 120 {
            return Err(fail(
                "Provide exactly one of prompt or task_id; poll_seconds must be 0..120",
            ));
        }
        let receipt_dir = workspace.join(".native-media");
        std::fs::create_dir_all(&receipt_dir)
            .map_err(|_| fail("Cannot create media receipt directory"))?;
        let receipt_dir = receipt_dir
            .canonicalize()
            .map_err(|_| fail("Cannot resolve media receipts"))?;
        let root = workspace
            .canonicalize()
            .map_err(|_| fail("Cannot resolve workspace"))?;
        if !receipt_dir.starts_with(root) {
            return Err(fail("Media receipts must remain inside workspace"));
        }
        let receipt = receipt_dir.join(format!("{}.json", uuid::Uuid::new_v4()));
        let mut id = args.task_id;
        let response = if let Some(id) = &id {
            valid_id(id)?;
            route.request(client,Method::GET,&format!("videos/{id}"))?.send().await
        } else {
            let prompt = args.prompt.as_ref().unwrap(); nonempty(prompt)?;
            let seconds = args.seconds.as_deref().unwrap_or("4");
            if !["4","8","12"].contains(&seconds) { return Err(fail("Unsupported video duration")); }
            write_receipt(&receipt,&json!({"status":"outcome_unknown","model":route.model,"output_path":args.output_path}))?;
            route.request(client,Method::POST,"videos")?.json(&json!({"model":route.model,"prompt":prompt,"seconds":seconds,"size":args.size.as_deref().unwrap_or("1280x720")})).send().await
        }.map_err(|_| fail("Video request outcome unknown; inspect .native-media receipts; not retried"))?;
        let mut payload: Value = parse_response(response).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(args.poll_seconds.into());
        loop {
            let returned_id = payload["id"].as_str().ok_or_else(|| {
                fail("Video response missing job ID; inspect receipts before resubmitting")
            })?;
            valid_id(returned_id)?;
            if id.as_ref().is_some_and(|id| id != returned_id) {
                return Err(fail("Video response job ID mismatch"));
            }
            id = Some(returned_id.into());
            let status = payload["status"].as_str().filter(|status| {
                matches!(
                    *status,
                    "queued" | "in_progress" | "completed" | "failed" | "cancelled" | "expired"
                )
            });
            // Persist ID before validating status or fetching content, so any later
            // protocol/decode failure remains recoverable without another POST.
            write_receipt(
                &receipt,
                &json!({"task_id":returned_id,"status":status.unwrap_or("unrecognized"),"output_path":args.output_path,"model":route.model}),
            )?;
            let status = status.ok_or_else(|| {
                fail("Missing or unsupported OpenAI video status; job ID persisted")
            })?;
            let mut result = json!({"task_id":returned_id,"status":status,"receipt":format!(".native-media/{}",receipt.file_name().unwrap().to_string_lossy())});
            match status {
                "completed" => {
                    let response = route
                        .request(
                            client,
                            Method::GET,
                            &format!("videos/{returned_id}/content"),
                        )?
                        .send()
                        .await
                        .map_err(|_| {
                            fail("Video content fetch failed; resume persisted task_id")
                        })?;
                    let bytes = response_bytes(response, 256 * 1024 * 1024).await?;
                    decode(&self.ffmpeg_executable, &bytes, "mp4").await?;
                    let artifact = publish(&output, &args.output_path, &bytes, "video/mp4")?;
                    result
                        .as_object_mut()
                        .unwrap()
                        .extend(artifact.as_object().unwrap().clone());
                    return Ok(result);
                }
                "failed" | "cancelled" | "expired" => return Ok(result),
                "queued" | "in_progress" => {}
                _ => return Err(fail("Unsupported OpenAI video status; job ID persisted")),
            }
            if let Some(progress) = payload["progress"]
                .as_f64()
                .filter(|v| (0.0..=100.0).contains(v))
            {
                result["progress"] = json!(progress);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(result);
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                tokio::time::Instant::now() + Duration::from_secs(2),
            ))
            .await;
            if tokio::time::Instant::now() >= deadline {
                return Ok(result);
            }
            let request = route.request(client, Method::GET, &format!("videos/{returned_id}"))?;
            let next = tokio::time::timeout_at(deadline, async {
                parse_response(
                    request
                        .send()
                        .await
                        .map_err(|_| fail("Video polling failed; resume persisted task_id"))?,
                )
                .await
            })
            .await;
            match next {
                Ok(value) => payload = value?,
                Err(_) => return Ok(result),
            }
        }
    }
}

impl MediaRoute {
    fn validate(&self, expected: MediaEndpoint) -> Result<(), Error> {
        if self.endpoint != expected {
            return Err(fail("Media endpoint does not match route kind"));
        }
        nonempty(&self.model)?;
        let url =
            reqwest::Url::parse(&self.base_url).map_err(|_| fail("Invalid media base URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(fail(
                "Media base URL requires HTTP(S) without credentials, query or fragment",
            ));
        }
        for (key, value) in &self.headers {
            reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| fail("Invalid media header name"))?;
            reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| fail("Invalid media header value"))?;
            if [
                "host",
                "content-length",
                "content-type",
                "transfer-encoding",
                "connection",
            ]
            .contains(&key.to_ascii_lowercase().as_str())
            {
                return Err(fail("Transport headers cannot be overridden"));
            }
            if key.eq_ignore_ascii_case("authorization") && self.bearer_token.is_some() {
                return Err(fail(
                    "Configure bearer token or Authorization header, not both",
                ));
            }
        }
        if let Some(token) = &self.bearer_token {
            nonempty(token)?;
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| fail("Invalid bearer token"))?;
        }
        Ok(())
    }
    fn request(
        &self,
        client: &Client,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let mut request = client.request(
            method,
            format!("{}/{path}", self.base_url.trim_end_matches('/')),
        );
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token);
        }
        Ok(request)
    }
}

async fn response_bytes(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, Error> {
    if !response.status().is_success() {
        return Err(fail(&format!(
            "Media route returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| fail("Media response interrupted; not retried"))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(fail("Media response exceeds byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn parse_response(response: reqwest::Response) -> Result<Value, Error> {
    serde_json::from_slice(&response_bytes(response, 1024 * 1024).await?)
        .map_err(|_| fail("Invalid video response JSON; inspect receipts"))
}
fn valid_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Err(fail("Invalid video job ID"))
    } else {
        Ok(())
    }
}
fn relative(value: &str) -> Result<&Path, Error> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || !path.components().all(|p| matches!(p, Component::Normal(_)))
    {
        return Err(fail(
            "Artifact path must be workspace relative without traversal",
        ));
    }
    Ok(path)
}
fn input_path(root: &Path, value: &str) -> Result<PathBuf, Error> {
    let root = root
        .canonicalize()
        .map_err(|_| fail("Cannot resolve workspace"))?;
    let path = root
        .join(relative(value)?)
        .canonicalize()
        .map_err(|_| fail("Cannot resolve reference"))?;
    if !path.starts_with(root) {
        return Err(fail("Reference escapes workspace"));
    }
    if !path.is_file() {
        return Err(fail("Image reference must be a regular file"));
    }
    Ok(path)
}
fn output_path(root: &Path, value: &str, extension: &str) -> Result<PathBuf, Error> {
    let path = relative(value)?;
    if path.extension().and_then(|v| v.to_str()) != Some(extension) {
        return Err(fail("Artifact output extension does not match media type"));
    }
    let root = root
        .canonicalize()
        .map_err(|_| fail("Cannot resolve workspace"))?;
    let destination = root.join(path);
    let parent = destination
        .parent()
        .unwrap()
        .canonicalize()
        .map_err(|_| fail("Output parent must already exist"))?;
    if !parent.starts_with(root) || destination.symlink_metadata().is_ok() {
        return Err(fail("Output escapes workspace or already exists"));
    }
    Ok(parent.join(path.file_name().unwrap()))
}
fn publish(path: &Path, relative: &str, bytes: &[u8], mime: &str) -> Result<Value, Error> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())
        .map_err(|_| fail("Cannot stage artifact"))?;
    file.write_all(bytes)
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| fail("Cannot write artifact"))?;
    file.persist_noclobber(path)
        .map_err(|_| fail("Cannot publish artifact; output may already exist"))?;
    Ok(
        json!({"artifact":{"path":relative,"mimeType":mime,"bytes":bytes.len(),"revision":revision(bytes)},"revision":revision(bytes),"reviewRequired":true}),
    )
}
fn write_receipt(path: &Path, value: &Value) -> Result<(), Error> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())
        .map_err(|_| fail("Cannot stage video receipt"))?;
    file.write_all(value.to_string().as_bytes())
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| fail("Cannot persist video receipt"))?;
    file.persist(path)
        .map_err(|_| fail("Cannot publish video receipt"))?;
    Ok(())
}
async fn decoder_available(executable: &Path) -> Result<(), Error> {
    let status = tokio::process::Command::new(executable)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|_| fail("ffmpeg decoder unavailable; no paid request submitted"))?;
    if !status.success() {
        return Err(fail(
            "ffmpeg decoder unavailable; no paid request submitted",
        ));
    }
    Ok(())
}
async fn decode(executable: &Path, bytes: &[u8], kind: &str) -> Result<(), Error> {
    let valid = match kind {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "mp3" => {
            bytes.starts_with(b"ID3")
                || bytes
                    .get(..2)
                    .is_some_and(|b| b[0] == 0xff && b[1] & 0xe0 == 0xe0)
        }
        "mp4" => bytes.get(4..8) == Some(b"ftyp"),
        _ => false,
    };
    if !valid {
        return Err(fail("Response is not the requested media format"));
    }
    let mut file = tempfile::NamedTempFile::new().map_err(|_| fail("Cannot stage media decode"))?;
    file.write_all(bytes)
        .map_err(|_| fail("Cannot stage media decode"))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args([
            "-nostdin",
            "-v",
            "error",
            "-xerror",
            "-progress",
            "pipe:1",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ])
        .arg(file.path())
        .args([
            "-map",
            if kind == "mp3" { "0:a:0" } else { "0:v:0" },
            "-f",
            "null",
            "-",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .map_err(|_| fail("Media decode exceeded time limit"))?
        .map_err(|_| fail("Cannot execute media decoder"))?;
    let counter = if kind == "mp3" {
        "out_time_us="
    } else {
        "frame="
    };
    let decoded = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        line.strip_prefix(counter)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .is_some_and(|value| value > 0)
    });
    if !output.status.success() || !decoded {
        return Err(fail("Media failed actual decoder validation"));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn selected_ffmpeg_handles_preflight_and_decode_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("selected ffmpeg");
        let calls = root.path().join("calls");
        std::fs::write(&executable, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\nif [ \"$1\" = '-version' ]; then exit 0; fi\nexit 23\n",
            calls.display()
        )).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        decoder_available(&executable).await.unwrap();
        assert!(
            decode(&executable, b"\x89PNG\r\n\x1a\n", "png")
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            "-version\n-nostdin\n"
        );
        std::fs::remove_file(&executable).unwrap();
        assert!(decoder_available(&executable).await.is_err());
    }
}
