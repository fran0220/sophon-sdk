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
        ffmpeg_executable: PathBuf,
        mut frames: broadcast::Receiver<Value>,
        scope: ProcessScope,
        mut initial: Option<Value>,
    ) -> Result<Self, Error> {
        let status = Command::new(&ffmpeg_executable)
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
        let published = root.join(format!("{id}.mp4"));
        let output = directory.join("output.mp4");
        let task = tokio::spawn(async move {
            let start = Instant::now();
            let mut timestamps = Vec::new();
            let mut bytes_written = 0usize;
            loop {
                tokio::select! {
                    biased;
                    _ = stop_task.notified() => break,
                    _ = tokio::time::sleep_until((start + Duration::from_secs(300)).into()) => return Err(Error::Invalid("recording exceeds 5 minute limit".into())),
                    frame = async { if let Some(frame) = initial.take() { Ok(frame) } else { frames.recv().await } } => {
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
            let mut command = Command::new(&ffmpeg_executable);
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
            tokio::fs::rename(output, published).await?;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn selected_ffmpeg_handles_preflight_and_encoding_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("selected ffmpeg");
        let calls = root.path().join("calls");
        std::fs::write(&executable, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\nif [ \"$1\" = '-version' ]; then exit 0; fi\nexit 23\n",
            calls.display()
        )).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_frames, rx) = broadcast::channel(1);
        let mut recording = Recording::start(
            "tab".into(),
            root.path().into(),
            executable.clone(),
            rx,
            ProcessScope::new(),
            Some(serde_json::json!({"tab_id":"tab","base64":"eA=="})),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !recording.dir.join("0.jpg").is_file() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(recording.finish().await, Err(Error::Protocol(_))));
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            "-version\n-nostdin\n"
        );
        std::fs::remove_file(&executable).unwrap();
        let (_frames, rx) = broadcast::channel(1);
        assert!(matches!(
            Recording::start(
                "tab".into(),
                root.path().into(),
                executable,
                rx,
                ProcessScope::new(),
                None,
            )
            .await,
            Err(Error::Unsupported(_))
        ));
    }
}
