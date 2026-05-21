//! Per-program subprocess: spawn, log capture, lifecycle.
//!
//! Every program gets one [`ProcessHandle`]. Spawning runs
//! `cargo run -p <pkg> -- <argv>` in `working_dir`. stdout + stderr are
//! line-buffered into a bounded `VecDeque<LogLine>` so the TUI can display a
//! live tail.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const LOG_CAP: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    System,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum Status {
    Stopped,
    Running { pid: Option<u32> },
    Exited { code: Option<i32> },
}

/// Messages emitted from the spawned task back to the UI loop.
///
/// Only `Exited` carries info the UI cares about today; `Started`/`Line` are
/// emitted as wake-ups so the render tick doesn't have to be aggressive.
#[derive(Debug)]
#[allow(dead_code)]
pub enum ProcEvent {
    Started { program_id: String, pid: Option<u32> },
    Line { program_id: String, line: LogLine },
    Exited { program_id: String, code: Option<i32> },
}

#[derive(Debug)]
pub struct ProcessHandle {
    pub program_id: String,
    pub status: Status,
    pub logs: Arc<Mutex<VecDeque<LogLine>>>,
    pub last_command: Option<String>,
    /// Set once the process is alive; `take()` it to kill.
    child: Arc<Mutex<Option<Child>>>,
    pump: Option<JoinHandle<()>>,
}

impl ProcessHandle {
    pub fn new(program_id: &str) -> Self {
        Self {
            program_id: program_id.to_string(),
            status: Status::Stopped,
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(LOG_CAP))),
            last_command: None,
            child: Arc::new(Mutex::new(None)),
            pump: None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, Status::Running { .. })
    }

    /// Spawn `cargo run -p <cargo_pkg> -- <argv>` in `working_dir`.
    pub fn spawn(
        &mut self,
        cargo_pkg: &str,
        working_dir: &str,
        argv: &[String],
        envs: &[(String, String)],
        events: mpsc::UnboundedSender<ProcEvent>,
    ) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }

        let mut cmd = Command::new("cargo");
        cmd.arg("run")
            .arg("-p")
            .arg(cargo_pkg)
            .arg("--quiet")
            .arg("--");
        for a in argv {
            cmd.arg(a);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.current_dir(working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);

        let env_prefix = if envs.is_empty() {
            String::new()
        } else {
            let joined = envs
                .iter()
                .map(|(k, v)| format!("{k}={}", shell_quote(v)))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{joined} ")
        };
        let display = format!(
            "{env_prefix}cargo run -p {cargo_pkg} -- {}",
            shell_join(argv)
        );
        self.last_command = Some(display.clone());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {cargo_pkg}"))?;
        let pid = child.id();
        self.status = Status::Running { pid };
        push_log(
            &self.logs,
            LogLine {
                stream: Stream::System,
                text: format!("$ {display}"),
            },
        );
        let _ = events.send(ProcEvent::Started {
            program_id: self.program_id.clone(),
            pid,
        });

        let stdout = child.stdout.take().context("missing stdout pipe")?;
        let stderr = child.stderr.take().context("missing stderr pipe")?;

        *self.child.lock().unwrap() = Some(child);

        let logs_out = Arc::clone(&self.logs);
        let logs_err = Arc::clone(&self.logs);
        let logs_sys = Arc::clone(&self.logs);
        let id_out = self.program_id.clone();
        let id_err = self.program_id.clone();
        let id_exit = self.program_id.clone();
        let events_out = events.clone();
        let events_err = events.clone();
        let events_exit = events.clone();
        let child_handle = Arc::clone(&self.child);

        let pump = tokio::spawn(async move {
            let stdout_task = pipe_lines(
                stdout,
                Stream::Stdout,
                logs_out,
                id_out,
                events_out,
            );
            let stderr_task = pipe_lines(
                stderr,
                Stream::Stderr,
                logs_err,
                id_err,
                events_err,
            );
            tokio::join!(stdout_task, stderr_task);

            // Reap the process once both pipes drain. The MutexGuard must
            // not be held across an `.await` (it isn't Send), so take the
            // child out first then await.
            let child_opt = child_handle.lock().unwrap().take();
            let code = if let Some(mut child) = child_opt {
                match child.try_wait() {
                    Ok(Some(s)) => s.code(),
                    _ => match child.wait().await {
                        Ok(s) => s.code(),
                        Err(_) => None,
                    },
                }
            } else {
                None
            };
            push_log(
                &logs_sys,
                LogLine {
                    stream: Stream::System,
                    text: format!(
                        "── exited (code {})",
                        code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                    ),
                },
            );
            let _ = events_exit.send(ProcEvent::Exited {
                program_id: id_exit,
                code,
            });
        });
        self.pump = Some(pump);
        Ok(())
    }

    /// Send SIGTERM (best-effort; on Unix this is what `kill()` does in
    /// tokio). Drop also kills via `kill_on_drop`, but explicit stop is
    /// nicer for the UI.
    pub async fn stop(&mut self) -> Result<()> {
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        Ok(())
    }

    pub fn mark_exited(&mut self, code: Option<i32>) {
        self.status = Status::Exited { code };
    }

    pub fn snapshot_logs(&self) -> Vec<LogLine> {
        self.logs.lock().unwrap().iter().cloned().collect()
    }
}

async fn pipe_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    stream: Stream,
    logs: Arc<Mutex<VecDeque<LogLine>>>,
    program_id: String,
    events: mpsc::UnboundedSender<ProcEvent>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        let line = LogLine { stream, text };
        push_log(&logs, line.clone());
        let _ = events.send(ProcEvent::Line {
            program_id: program_id.clone(),
            line,
        });
    }
}

fn push_log(buf: &Arc<Mutex<VecDeque<LogLine>>>, line: LogLine) {
    let mut g = buf.lock().unwrap();
    if g.len() == LOG_CAP {
        g.pop_front();
    }
    g.push_back(line);
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
