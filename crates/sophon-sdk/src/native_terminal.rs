//! Client PTYs, separate from agent shell tools. Uses upstream ptyctl spawning and
//! the same process-group supervisor as xai-grok-shell-terminal.
//!
//! Subscribe before opening. Output is base64, in chunks of at most 4096 bytes;
//! `offset` is the exclusive byte offset. The 256-event broadcast is bounded:
//! receivers MUST report `RecvError::Lagged` as lost events, never silently retry.
//! Exit proves the direct child was reaped, not that all output has been drained.
//! Unix cleanup follows upstream terminal semantics: HUP the shell (which forwards
//! HUP to its jobs), then kill its process group. Disowned/daemonized jobs are not
//! contained. No product permission layer is imposed over the OS user's authority.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ptyctl::pty::{PtyConfig, PtyHandle, PtyMaster};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use ts_rs::TS;

const MAX_TERMINALS: usize = 32;
const MAX_INPUT: usize = 64 * 1024;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TerminalRequest {
    Open {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        cwd: PathBuf,
        #[serde(default)]
        env: HashMap<String, String>,
        cols: u16,
        rows: u16,
    },
    /// Base64-encoded bytes, including control keys and escape sequences (64 KiB max).
    /// Success confirms a write/flush, not that the program processed the input.
    Write {
        terminal_id: String,
        data: String,
    },
    Resize {
        terminal_id: String,
        cols: u16,
        rows: u16,
    },
    Close {
        terminal_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TerminalEvent {
    Output {
        terminal_id: String,
        data: String,
        #[ts(type = "number")]
        offset: u64,
    },
    Exit {
        terminal_id: String,
        exit_code: u32,
    },
    Error {
        terminal_id: String,
        message: String,
    },
}

type WriteResult = Result<(), String>;
type ExitResult = Result<u32, String>;

struct Input {
    data: Vec<u8>,
    reply: oneshot::Sender<WriteResult>,
}

struct Terminal {
    master: PtyMaster,
    input: mpsc::Sender<Input>,
    closing: Arc<AtomicBool>,
    exit: watch::Receiver<Option<ExitResult>>,
}

#[derive(Default)]
struct State {
    stopped: bool,
    terminals: HashMap<String, Arc<Terminal>>,
}

/// Own one service per Runtime. Call `shutdown` and await its result before stopping
/// the async runtime. Drop requests cleanup but cannot certify its completion.
pub struct NativeTerminalService {
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<TerminalEvent>,
}

fn error(message: impl ToString) -> crate::Error {
    crate::Error::Operation(format!("terminal: {}", message.to_string()))
}

impl Default for NativeTerminalService {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeTerminalService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            events: broadcast::channel(256).0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TerminalEvent> {
        self.events.subscribe()
    }

    pub async fn execute(&self, request: TerminalRequest) -> Result<Value, crate::Error> {
        match request {
            TerminalRequest::Open {
                program,
                args,
                cwd,
                env,
                cols,
                rows,
            } => {
                validate_size(cols, rows)?;
                if program.is_empty()
                    || program.contains('\0')
                    || !cwd.is_absolute()
                    || args.iter().any(|s| s.contains('\0'))
                    || env
                        .iter()
                        .any(|(k, v)| k.is_empty() || k.contains(['\0', '=']) || v.contains('\0'))
                {
                    return Err(error(
                        "program/arguments/environment must be valid; cwd must be absolute",
                    ));
                }
                let state = self.state.clone();
                let events = self.events.clone();
                // The blocking closure owns registration even if this request is cancelled.
                // Registration and shutdown share a lock, so a spawn cannot escape shutdown.
                tokio::task::spawn_blocking(move || {
                    let mut state = state.lock().map_err(error)?;
                    if state.stopped {
                        return Err(error("service shut down"));
                    }
                    if state.terminals.len() >= MAX_TERMINALS {
                        return Err(error("terminal limit reached; close exited terminals"));
                    }
                    let mut command = vec![program];
                    command.extend(args);
                    let id = uuid::Uuid::now_v7().to_string();
                    let terminal = start_terminal(
                        &id,
                        PtyConfig {
                            command,
                            cols,
                            rows,
                            cwd: Some(cwd),
                            env,
                        },
                        events,
                    )?;
                    state.terminals.insert(id.clone(), Arc::new(terminal));
                    Ok(json!({ "terminalId": id }))
                })
                .await
                .map_err(error)?
            }
            TerminalRequest::Write { terminal_id, data } => {
                if data.len() > MAX_INPUT.div_ceil(3) * 4 {
                    return Err(error("input exceeds 64 KiB"));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(error)?;
                if bytes.len() > MAX_INPUT {
                    return Err(error("input exceeds 64 KiB"));
                }
                let terminal = self.terminal(&terminal_id)?;
                ensure_running(&terminal)?;
                let (reply, response) = oneshot::channel();
                terminal
                    .input
                    .try_send(Input { data: bytes, reply })
                    .map_err(error)?;
                // A timeout is an uncertain write, not proof that no bytes were written.
                tokio::time::timeout(CLOSE_TIMEOUT, response)
                    .await
                    .map_err(|_| {
                        error("write timed out; bytes may have been written, do not blindly retry")
                    })?
                    .map_err(error)?
                    .map_err(error)?;
                Ok(json!({}))
            }
            TerminalRequest::Resize {
                terminal_id,
                cols,
                rows,
            } => {
                validate_size(cols, rows)?;
                let terminal = self.terminal(&terminal_id)?;
                ensure_running(&terminal)?;
                tokio::task::spawn_blocking(move || terminal.master.resize(cols, rows))
                    .await
                    .map_err(error)?
                    .map_err(error)?;
                Ok(json!({}))
            }
            TerminalRequest::Close { terminal_id } => {
                let terminal = self.terminal(&terminal_id)?;
                terminal.closing.store(true, Ordering::Release);
                let exit_code = await_exit(terminal.exit.clone()).await?;
                self.state
                    .lock()
                    .map_err(error)?
                    .terminals
                    .remove(&terminal_id);
                Ok(json!({ "terminalId": terminal_id, "exitCode": exit_code }))
            }
        }
    }

    fn terminal(&self, id: &str) -> Result<Arc<Terminal>, crate::Error> {
        let state = self.state.lock().map_err(error)?;
        state
            .terminals
            .get(id)
            .cloned()
            .ok_or_else(|| error(format!("unknown terminal {id}")))
    }

    pub async fn shutdown(&self) -> Result<(), crate::Error> {
        let terminals = {
            let mut state = self.state.lock().map_err(error)?;
            state.stopped = true;
            for terminal in state.terminals.values() {
                terminal.closing.store(true, Ordering::Release);
            }
            state
                .terminals
                .iter()
                .map(|(id, terminal)| (id.clone(), terminal.clone()))
                .collect::<Vec<_>>()
        };
        let mut failures = Vec::new();
        for (id, terminal) in terminals {
            match await_exit(terminal.exit.clone()).await {
                Ok(_) => {
                    self.state.lock().map_err(error)?.terminals.remove(&id);
                }
                Err(e) => failures.push(format!("{id}: {e}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(error(failures.join("; ")))
        }
    }
}

impl Drop for NativeTerminalService {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            for terminal in state.terminals.values() {
                terminal.closing.store(true, Ordering::Release);
            }
        }
    }
}

fn validate_size(cols: u16, rows: u16) -> Result<(), crate::Error> {
    if cols == 0 || rows == 0 {
        Err(error("rows and cols must be nonzero"))
    } else {
        Ok(())
    }
}

fn ensure_running(terminal: &Terminal) -> Result<(), crate::Error> {
    if terminal.closing.load(Ordering::Acquire) || terminal.exit.borrow().is_some() {
        Err(error("terminal closing or exited"))
    } else {
        Ok(())
    }
}

async fn await_exit(mut exit: watch::Receiver<Option<ExitResult>>) -> Result<u32, crate::Error> {
    tokio::time::timeout(CLOSE_TIMEOUT, async {
        loop {
            if let Some(result) = exit.borrow().clone() {
                return result.map_err(error);
            }
            exit.changed().await.map_err(error)?;
        }
    })
    .await
    .map_err(|_| error("exit not observed within deadline; cleanup remains active"))?
}

fn start_terminal(
    id: &str,
    config: PtyConfig,
    events: broadcast::Sender<TerminalEvent>,
) -> Result<Terminal, crate::Error> {
    let (master, mut child, mut reader, mut writer) =
        PtyHandle::spawn(&config).map_err(error)?.into_parts();
    let scope = xai_tty_utils::ProcessScope::new();
    let group = match child
        .pid()
        .ok_or_else(|| error("PTY child has no PID"))
        .and_then(|pid| scope.enroll_terminal_pid(pid).map_err(error))
    {
        Ok(group) => group,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let closing = Arc::new(AtomicBool::new(false));
    let (input, mut input_rx) = mpsc::channel::<Input>(16);
    let (exit_tx, exit) = watch::channel(None);
    let id = id.to_owned();

    // Dedicated OS threads outlive Tokio shutdown and never block an async worker.
    // The output broadcast never blocks the reader, even with a stalled client.
    let output_events = events.clone();
    let output_id = id.clone();
    std::thread::spawn(move || {
        let mut offset = 0_u64;
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    offset += n as u64;
                    let _ = output_events.send(TerminalEvent::Output {
                        terminal_id: output_id.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(&buffer[..n]),
                        offset,
                    });
                }
                // Linux reports EIO when the slave closes, rather than EOF.
                #[cfg(target_os = "linux")]
                Err(e) if e.raw_os_error() == Some(5) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = output_events.send(TerminalEvent::Error {
                        terminal_id: output_id,
                        message: format!("PTY read: {e}"),
                    });
                    break;
                }
            }
        }
    });
    let input_closing = closing.clone();
    let input_events = events.clone();
    let input_id = id.clone();
    std::thread::spawn(move || {
        while let Some(input) = input_rx.blocking_recv() {
            let result = if input_closing.load(Ordering::Acquire) {
                Err("terminal closing or exited".to_owned())
            } else {
                writer
                    .write_all(&input.data)
                    .and_then(|()| writer.flush())
                    .map_err(|e| format!("PTY write: {e}"))
            };
            if let Err(message) = &result {
                let _ = input_events.send(TerminalEvent::Error {
                    terminal_id: input_id.clone(),
                    message: message.clone(),
                });
            }
            let failed = result.is_err();
            let _ = input.reply.send(result);
            if failed {
                break;
            }
        }
    });
    let worker_closing = closing.clone();
    std::thread::spawn(move || {
        let mut requested_at = None;
        let mut killed = false;
        loop {
            // PtyChild::is_alive only returns false after a successful try_wait.
            // Do not signal the process group once the child is observed exited.
            if !child.is_alive() {
                break;
            }
            if worker_closing.load(Ordering::Acquire) {
                let started = requested_at.get_or_insert_with(|| {
                    if let Err(e) = group.hangup() {
                        let _ = events.send(TerminalEvent::Error {
                            terminal_id: id.clone(),
                            message: format!("PTY hangup: {e}"),
                        });
                    }
                    Instant::now()
                });
                if !killed && started.elapsed() >= xai_tty_utils::HANGUP_GRACE {
                    if let Err(e) = group.kill() {
                        let _ = events.send(TerminalEvent::Error {
                            terminal_id: id.clone(),
                            message: format!("PTY kill: {e}"),
                        });
                    }
                    killed = true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Capture the direct child's status while its Windows Job is still
        // alive. Closing a kill-on-close Job before this wait can change the
        // reported ConPTY status (observed 7 -> 0). Cleanup still runs before
        // publishing either the exit receipt or a wait error.
        let result = child.wait().map_err(|e| format!("PTY wait: {e}"));
        drop(group);
        drop(scope);
        worker_closing.store(true, Ordering::Release);
        match &result {
            Ok(code) => {
                let _ = events.send(TerminalEvent::Exit {
                    terminal_id: id,
                    exit_code: *code,
                });
            }
            Err(message) => {
                let _ = events.send(TerminalEvent::Error {
                    terminal_id: id,
                    message: message.clone(),
                });
            }
        }
        let _ = exit_tx.send(Some(result));
    });
    Ok(Terminal {
        master,
        input,
        closing,
        exit,
    })
}

#[cfg(test)]
mod windows_tests {
    #[cfg(windows)]
    use super::*;

    fn cursor_queries(pending: &mut Vec<u8>, chunk: &[u8]) -> usize {
        pending.extend_from_slice(chunk);
        let queries = pending
            .windows(4)
            .filter(|bytes| *bytes == b"\x1b[6n")
            .count();
        // Retain only a possible split query. Completed queries cannot match again.
        pending.drain(..pending.len().saturating_sub(3));
        queries
    }

    #[test]
    fn cursor_queries_survive_chunk_boundaries_without_duplicate_replies() {
        let mut pending = Vec::new();
        assert_eq!(cursor_queries(&mut pending, b"startup\x1b["), 0);
        assert_eq!(cursor_queries(&mut pending, b"6ntext\x1b[6n\x1b"), 2);
        assert_eq!(cursor_queries(&mut pending, b"[6"), 0);
        assert_eq!(cursor_queries(&mut pending, b"n"), 1);
        assert_eq!(cursor_queries(&mut pending, b"normal output"), 0);
    }

    /// Exercise actual interactive ConPTY processes concurrently: the direct
    /// child's code must survive kill-on-close Job teardown, not become zero.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conpty_preserves_direct_exit_before_job_teardown() {
        let program = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut workers = tokio::task::JoinSet::new();
        for code in [3_u32, 7, 19, 23] {
            let program = program.clone();
            workers.spawn(async move {
                let service = NativeTerminalService::new();
                for _ in 0..8 {
                    let mut events = service.subscribe();
                    let opened = service.execute(TerminalRequest::Open {
                        program: program.to_string_lossy().into_owned(), args: vec![],
                        cwd: std::env::temp_dir(), env: HashMap::new(), cols: 80, rows: 24,
                    }).await.expect("open ConPTY");
                    let id = opened["terminalId"].as_str().unwrap().to_owned();
                    service.execute(TerminalRequest::Resize { terminal_id:id.clone(), cols:103, rows:37 }).await.expect("resize");
                    let command = format!("Write-Host ('EXITPID=' + $PID + ':{code}'); [Environment]::Exit({code})\r\n");
                    service.execute(TerminalRequest::Write { terminal_id:id.clone(), data:base64::engine::general_purpose::STANDARD.encode(command) }).await.expect("write");
                    tokio::time::timeout(Duration::from_secs(30), async {
                        let mut pending_output = Vec::new();
                        loop {
                            match events.recv().await.expect("terminal event") {
                                TerminalEvent::Exit {terminal_id,exit_code} if terminal_id == id => {
                                    assert_eq!(exit_code,code,"ConPTY direct exit changed during cleanup");
                                    break;
                                }
                                TerminalEvent::Error {terminal_id,message} if terminal_id == id => panic!("{message}"),
                                TerminalEvent::Output {terminal_id,data,..} if terminal_id == id => {
                                    let bytes = base64::engine::general_purpose::STANDARD.decode(data).expect("output base64");
                                    for _ in 0..cursor_queries(&mut pending_output,&bytes) {
                                        service.execute(TerminalRequest::Write {
                                            terminal_id:id.clone(),
                                            data:base64::engine::general_purpose::STANDARD.encode(b"\x1b[1;1R"),
                                        }).await.expect("reply to ConPTY cursor query");
                                    }
                                }
                                // Exit is not an output-drain barrier. Previous
                                // terminals may still emit on this service bus.
                                _ => {}
                            }
                        }
                    }).await.expect("exit deadline");
                    // Both published event and retained close receipt must keep
                    // the original code; host close only follows the assertion.
                    let closed = service.execute(TerminalRequest::Close {terminal_id:id}).await.expect("close");
                    assert_eq!(closed["exitCode"],code);
                }
                service.shutdown().await.expect("shutdown");
            });
        }
        while let Some(result) = workers.join_next().await {
            result.expect("ConPTY worker");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(args: &[&str]) -> TerminalRequest {
        TerminalRequest::Open {
            program: "/bin/bash".into(),
            args: args.iter().map(|s| (*s).into()).collect(),
            cwd: std::env::temp_dir(),
            env: HashMap::from([("LC_ALL".into(), "C".into())]),
            cols: 81,
            rows: 23,
        }
    }

    async fn open(service: &NativeTerminalService, args: &[&str]) -> String {
        service.execute(shell(args)).await.expect("open")["terminalId"]
            .as_str()
            .expect("id")
            .into()
    }

    async fn write(service: &NativeTerminalService, id: &str, data: &[u8]) {
        service
            .execute(TerminalRequest::Write {
                terminal_id: id.into(),
                data: base64::engine::general_purpose::STANDARD.encode(data),
            })
            .await
            .expect("write");
    }

    async fn output_until(events: &mut broadcast::Receiver<TerminalEvent>, needle: &str) -> String {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut text = String::new();
            loop {
                match events.recv().await.expect("event") {
                    TerminalEvent::Output { data, .. } => {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .expect("base64");
                        text.push_str(&String::from_utf8_lossy(&bytes));
                        if text.contains(needle) {
                            return text;
                        }
                    }
                    TerminalEvent::Error { message, .. } => panic!("{message}"),
                    TerminalEvent::Exit { .. } => {}
                }
            }
        })
        .await
        .expect("output deadline")
    }

    #[tokio::test]
    async fn actual_input_resize_and_exit_status() {
        let service = NativeTerminalService::new();
        let mut events = service.subscribe();
        let mut exit_events = service.subscribe();
        let id = open(&service, &["--noprofile", "--norc", "-c", "stty -echo; printf READY; read line; printf 'INPUT:%s\\n' \"$line\"; stty size; exit 7"]).await;
        output_until(&mut events, "READY").await;
        service
            .execute(TerminalRequest::Resize {
                terminal_id: id.clone(),
                cols: 103,
                rows: 37,
            })
            .await
            .expect("resize");
        write(&service, &id, b"asymmetric-input\n").await;
        let text = output_until(&mut events, "37 103").await;
        assert!(text.contains("INPUT:asymmetric-input"), "{text}");
        let code = await_exit(service.terminal(&id).expect("terminal").exit.clone())
            .await
            .expect("exit");
        assert_eq!(code, 7);
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut offset = 0;
            loop {
                match exit_events.recv().await.expect("event") {
                    TerminalEvent::Output {
                        terminal_id,
                        data,
                        offset: end,
                    } => {
                        assert_eq!(terminal_id, id);
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .expect("base64");
                        assert!(bytes.len() <= 4096);
                        assert_eq!(end, offset + bytes.len() as u64);
                        offset = end;
                    }
                    TerminalEvent::Exit {
                        terminal_id,
                        exit_code,
                    } => {
                        assert_eq!(terminal_id, id);
                        assert_eq!(exit_code, 7);
                        break;
                    }
                    TerminalEvent::Error { message, .. } => panic!("{message}"),
                }
            }
        })
        .await
        .expect("exit event deadline");
        assert!(
            service
                .execute(TerminalRequest::Write {
                    terminal_id: id.clone(),
                    data: "YQ==".into()
                })
                .await
                .is_err()
        );
        let closed = service
            .execute(TerminalRequest::Close { terminal_id: id })
            .await
            .expect("close");
        assert_eq!(closed["exitCode"], 7);
        service.shutdown().await.expect("shutdown");
    }

    #[cfg(target_os = "linux")]
    async fn wait_pid_gone(pid: u32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("process must be reaped, not merely signalled");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn close_reaps_shell_and_job_control_background_child() {
        let service = NativeTerminalService::new();
        let mut events = service.subscribe();
        let id = open(&service, &["--noprofile", "--norc", "-i"]).await;
        write(
            &service,
            &id,
            b"sleep 300 & printf '\\nCHILD:%s:PARENT:%s:END\\n' $! $$\n",
        )
        .await;
        let text = output_until(&mut events, ":END\r\n").await;
        let record = text
            .lines()
            .find(|line| line.starts_with("CHILD:"))
            .expect("pid line");
        let parts = record.split(':').collect::<Vec<_>>();
        let child = parts[1].parse::<u32>().expect("child pid");
        let parent = parts[3].parse::<u32>().expect("parent pid");
        assert!(std::path::Path::new(&format!("/proc/{child}")).exists());
        let process_group = |pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("process stat");
            stat.rsplit_once(')')
                .expect("comm")
                .1
                .split_whitespace()
                .nth(2)
                .expect("pgrp")
                .to_owned()
        };
        assert_ne!(
            process_group(child),
            process_group(parent),
            "exercise real job control, not only a shared group kill"
        );
        service
            .execute(TerminalRequest::Close { terminal_id: id })
            .await
            .expect("close");
        wait_pid_gone(parent).await;
        wait_pid_gone(child).await;
        service.shutdown().await.expect("shutdown");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shutdown_and_drop_cleanup_with_hangup_ignored() {
        for explicit in [true, false] {
            let service = NativeTerminalService::new();
            let mut events = service.subscribe();
            open(
                &service,
                &[
                    "-c",
                    "trap '' HUP; printf 'PID:%s:END\\n' $$; while :; do sleep 1; done",
                ],
            )
            .await;
            let text = output_until(&mut events, ":END").await;
            let pid = text
                .split("PID:")
                .nth(1)
                .expect("pid")
                .split(':')
                .next()
                .expect("digits")
                .parse()
                .expect("pid number");
            if explicit {
                service.shutdown().await.expect("shutdown");
                assert!(service.execute(shell(&["-c", "exit 0"])).await.is_err());
            }
            drop(service);
            wait_pid_gone(pid).await;
        }
    }

    #[tokio::test]
    async fn slow_consumer_reports_loss_and_does_not_block_exit() {
        let service = NativeTerminalService::new();
        let mut events = service.subscribe();
        let id = open(&service, &["-c", "head -c 2097152 /dev/zero; exit 9"]).await;
        assert_eq!(
            await_exit(service.terminal(&id).expect("terminal").exit.clone())
                .await
                .expect("exit"),
            9
        );
        assert!(
            matches!(events.recv().await, Err(broadcast::error::RecvError::Lagged(n)) if n > 0)
        );
        service.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn invalid_requests_and_failed_spawn_are_errors() {
        let service = NativeTerminalService::new();
        assert!(
            service
                .execute(TerminalRequest::Close {
                    terminal_id: "unknown".into()
                })
                .await
                .is_err()
        );
        let mut request = shell(&[]);
        if let TerminalRequest::Open { program, .. } = &mut request {
            *program = "/does/not/exist".into();
        }
        assert!(service.execute(request).await.is_err());
        let mut request = shell(&[]);
        if let TerminalRequest::Open { rows, .. } = &mut request {
            *rows = 0;
        }
        assert!(service.execute(request).await.is_err());
        service.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn missing_exit_evidence_is_not_success() {
        let (sender, exit) = watch::channel(None);
        assert!(
            await_exit(exit.clone())
                .await
                .expect_err("no observed exit")
                .to_string()
                .contains("exit not observed")
        );
        drop(sender);
        assert!(await_exit(exit).await.is_err());
    }
}
