//! Timestamped CDP frames encoded locally; no synthetic audio track.
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;
use xai_tty_utils::ProcessScope;

use crate::Error;

pub(crate) struct Recording {
    pub tab: String,
    pub id: String,
    dir: PathBuf,
    stop: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
    task: Option<JoinHandle<Result<(), Error>>>,
    result: Option<Result<(), Error>>,
}

impl Recording {
    pub async fn start(
        tab: String,
        root: PathBuf,
        mut frames: broadcast::Receiver<Value>,
        scope: ProcessScope,
    ) -> Result<Self, Error> {
        let status = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|_| {
                Error::Unsupported("recording requires ffmpeg on the Runtime host".into())
            })?;
        if !status.success() {
            return Err(Error::Unsupported("ffmpeg unavailable".into()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(format!("recording-{id}"));
        tokio::fs::create_dir_all(&dir).await?;
        let stop = Arc::new(Notify::new());
        let stop_task = stop.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = cancelled.clone();
        let directory = dir.clone();
        let target = tab.clone();
        let output = root.join(format!("{id}.mp4"));
        let task = tokio::spawn(async move {
            let start = Instant::now();
            let mut timestamps = Vec::new();
            let mut bytes_written = 0usize;
            loop {
                tokio::select! {
                    biased;
                    _ = stop_task.notified() => break,
                    _ = tokio::time::sleep_until((start + Duration::from_secs(300)).into()) => return Err(Error::Invalid("recording exceeds 5 minute limit".into())),
                    frame = frames.recv() => {
                        let frame = match frame {
                            Ok(frame) => frame,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => return Err(Error::Closed),
                        };
                        if frame["tab_id"] != target { continue; }
                        let received = start.elapsed().as_secs_f64();
                        let bytes = base64::engine::general_purpose::STANDARD.decode(frame["base64"].as_str().unwrap_or_default()).map_err(|e|Error::Protocol(e.to_string()))?;
                        bytes_written += bytes.len();
                        if bytes_written > 128 * 1024 * 1024 { return Err(Error::Invalid("recording exceeds 128 MiB frame limit".into())); }
                        tokio::fs::write(directory.join(format!("{}.jpg",timestamps.len())),bytes).await?;
                        timestamps.push(received);
                    }
                }
            }
            if cancellation.load(Ordering::Acquire) {
                return Err(Error::Closed);
            }
            if timestamps.is_empty() {
                return Err(Error::Invalid("recording received no frames".into()));
            }
            let elapsed = start.elapsed().as_secs_f64();
            // Hold the first and last images across quiet periods, preserving wall duration.
            timestamps[0] = 0.0;
            let mut manifest = String::from("ffconcat version 1.0\n");
            for (index, timestamp) in timestamps.iter().enumerate() {
                let end = timestamps.get(index + 1).copied().unwrap_or(elapsed);
                manifest.push_str(&format!(
                    "file '{index}.jpg'\nduration {:.6}\n",
                    (end - timestamp).max(0.001)
                ));
            }
            manifest.push_str(&format!("file '{}.jpg'\n", timestamps.len() - 1));
            tokio::fs::write(directory.join("frames.ffconcat"), manifest).await?;
            let mut command = Command::new("ffmpeg");
            command
                .args([
                    "-nostdin", "-v", "error", "-f", "concat", "-safe", "1", "-i",
                ])
                .arg(directory.join("frames.ffconcat"))
                .args([
                    "-an",
                    "-fps_mode",
                    "vfr",
                    "-vf",
                    "pad=ceil(iw/2)*2:ceil(ih/2)*2",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-movflags",
                    "+faststart",
                    "-n",
                ])
                .arg(&output)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let (mut child, group) = scope.spawn(command)?;
            let status = tokio::select! {
                _ = stop_task.notified() => {
                    group.kill()?;
                    child.wait().await?;
                    let _ = tokio::fs::remove_file(&output).await;
                    return Err(Error::Closed);
                }
                result = tokio::time::timeout(Duration::from_secs(45), child.wait()) => match result {
                    Ok(result) => result?,
                    Err(_) => { group.kill()?; child.wait().await?; let _ = tokio::fs::remove_file(&output).await; return Err(Error::Timeout); }
                },
            };
            if !status.success() {
                let _ = tokio::fs::remove_file(output).await;
                return Err(Error::Protocol(format!("ffmpeg failed: {status}")));
            }
            Ok(())
        });
        Ok(Self {
            tab,
            id,
            dir,
            stop,
            cancelled,
            task: Some(task),
            result: None,
        })
    }

    pub async fn finish(&mut self) -> Result<(), Error> {
        if self.result.is_none() {
            self.stop.notify_one();
            self.join().await;
        }
        self.cleanup().await?;
        self.result.take().unwrap_or(Ok(()))
    }

    pub async fn cancel(&mut self) -> Result<(), Error> {
        self.cancelled.store(true, Ordering::Release);
        self.stop.notify_one();
        self.join().await;
        self.cleanup().await
    }

    async fn join(&mut self) {
        if let Some(task) = self.task.as_mut() {
            self.result = Some(
                task.await
                    .unwrap_or_else(|e| Err(Error::Transport(e.to_string()))),
            );
            self.task.take();
        }
    }

    async fn cleanup(&self) -> Result<(), Error> {
        match tokio::fs::remove_dir_all(&self.dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
