//! PTY wrapper using `portable-pty` for cross-platform pseudoterminal support.

use std::collections::HashMap;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Configuration for spawning a PTY session.
#[derive(Debug, Clone)]
pub struct PtyConfig {
    /// Command and arguments to run.
    pub command: Vec<String>,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Additional environment variables.
    pub env: HashMap<String, String>,
}

/// Handle to a running PTY session.
pub struct PtyHandle {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

/// Resize-capable master half of a dismantled [`PtyHandle`].
/// portable-pty's unix master is not `Sync` (`RefCell`), so it sits behind a mutex
/// held only for the synchronous resize ioctl.
pub struct PtyMaster {
    master: std::sync::Mutex<Box<dyn MasterPty + Send>>,
}

impl PtyMaster {
    /// Resize the PTY (TIOCSWINSZ; the kernel delivers SIGWINCH to the child).
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")
    }
}

/// Child-process half of a dismantled [`PtyHandle`].
pub struct PtyChild {
    child: Box<dyn portable_pty::Child + Send>,
}

impl PtyChild {
    /// Check if the child process is still alive.
    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Get the child process ID.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Wait for the child to exit and return the exit code.
    pub fn wait(&mut self) -> Result<u32> {
        let status = self.child.wait().context("failed to wait for child")?;
        Ok(status.exit_code())
    }

    /// Kill the child process.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("failed to kill child process")
    }
}

impl PtyHandle {
    /// Spawn a new process in a PTY.
    pub fn spawn(config: &PtyConfig) -> Result<Self> {
        let pty_system = native_pty_system();

        let pty_size = PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(pty_size).context("failed to open PTY")?;

        let mut cmd = CommandBuilder::new(&config.command[0]);
        if config.command.len() > 1 {
            cmd.args(&config.command[1..]);
        }
        if let Some(ref cwd) = config.cwd {
            cmd.cwd(cwd);
        }
        #[cfg(unix)]
        {
            cmd.env_clear();
            for (key, value) in std::env::vars_os() {
                if key.as_bytes().contains(&0) || value.as_bytes().contains(&0) {
                    continue;
                }
                cmd.env(&key, &value);
            }
        }
        for (key, value) in &config.env {
            if key.contains('\0') || value.contains('\0') {
                continue;
            }
            cmd.env(key, value);
        }
        // Set TERM for proper terminal detection.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        // Not session-scoped: ptyctl's child is the process it exists to run.
        #[allow(clippy::disallowed_methods)]
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn command in PTY")?;

        Self::finish_spawn(pair.master, child)
    }

    /// The process already exists: every fallible stream acquisition must pass
    /// through this rollback boundary rather than dropping an unreaped child.
    fn finish_spawn(
        master: Box<dyn MasterPty + Send>,
        mut child: Box<dyn portable_pty::Child + Send>,
    ) -> Result<Self> {
        let streams = (|| {
            let reader = master
                .try_clone_reader()
                .context("failed to clone PTY reader")?;
            let writer = master.take_writer().context("failed to take PTY writer")?;
            Ok::<_, anyhow::Error>((reader, writer))
        })();
        let (reader, writer) = match streams {
            Ok(streams) => streams,
            Err(mut acquisition_error) => {
                // Do not replace the acquisition error with cleanup's result.
                // Wait even if kill fails: an already-exited child still needs reaping.
                if let Err(e) = child.kill() {
                    acquisition_error =
                        acquisition_error.context(format!("PTY rollback kill failed: {e}"));
                }
                if let Err(e) = child.wait() {
                    acquisition_error =
                        acquisition_error.context(format!("PTY rollback wait failed: {e}"));
                }
                return Err(acquisition_error);
            }
        };

        Ok(Self {
            master,
            child,
            reader,
            writer,
        })
    }

    /// Dismantle into the master half (kept for resize), the child half
    /// (moved into a waiter task), and the reader/writer streams.
    pub fn into_parts(
        self,
    ) -> (
        PtyMaster,
        PtyChild,
        Box<dyn Read + Send>,
        Box<dyn Write + Send>,
    ) {
        (
            PtyMaster {
                master: std::sync::Mutex::new(self.master),
            },
            PtyChild { child: self.child },
            self.reader,
            self.writer,
        )
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn stream_acquisition_failure_reaps_child() {
        const CHILD_MODE: &str = "PTYCTL_FD_EXHAUSTION_TEST";
        if std::env::var_os(CHILD_MODE).is_none() {
            // Change RLIMIT_NOFILE only in a fresh test process, never in the test
            // runner (whose other tests may be opening files concurrently).
            for spare in ["0", "1"] {
                let status = std::process::Command::new("/bin/sh")
                    .args(["-c", "ulimit -n 64; exec \"$1\" --exact pty::tests::stream_acquisition_failure_reaps_child --nocapture", "ptyctl-test"])
                    .arg(std::env::current_exe().expect("test executable"))
                    .env(CHILD_MODE, spare)
                    .status()
                    .expect("isolated test process");
                assert!(
                    status.success(),
                    "isolated FD-exhaustion test ({spare} spare descriptors)"
                );
            }
            return;
        }

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY before exhaustion");
        let mut command = CommandBuilder::new("/bin/sleep");
        command.arg("300");
        #[allow(clippy::disallowed_methods)]
        let child = pair.slave.spawn_command(command).expect("real child");
        let pid = child.process_id().expect("PID");
        drop(pair.slave);
        let mut descriptors = Vec::new();
        loop {
            match std::fs::File::open("/dev/null") {
                Ok(file) => descriptors.push(file),
                Err(e) => {
                    assert_eq!(e.raw_os_error(), Some(24), "must reach EMFILE");
                    break;
                }
            }
        }
        let spare = std::env::var(CHILD_MODE).expect("mode");
        if spare == "1" {
            descriptors.pop();
        }
        // With zero spare FDs the reader dup fails. With one spare, reader
        // acquisition succeeds and the writer dup fails. Neither is a mock.
        let error = match PtyHandle::finish_spawn(pair.master, child) {
            Ok(_) => panic!("stream acquisition unexpectedly succeeded"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        let stage = if spare == "0" {
            "failed to clone PTY reader"
        } else {
            "failed to take PTY writer"
        };
        assert!(
            message.contains(stage),
            "original acquisition error lost: {message}"
        );
        assert!(message.contains("Too many open files"), "{message}");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "spawned child was not reaped"
        );
        drop(descriptors);
    }
}
