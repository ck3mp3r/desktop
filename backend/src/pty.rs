use async_trait::async_trait;
use bytes::Bytes;
use eyre::{eyre, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::{
    task::spawn_blocking,
    time::{sleep, Duration, Instant},
};
use ts_rs::TS;
use uuid::Uuid;

use crate::runtime::pty_store::PtyLike;

#[derive(Clone, Deserialize, Serialize, Debug, TS)]
#[ts(export)]
pub struct PtyMetadata {
    pub pid: Uuid,
    pub runbook: Uuid,
    pub block: String,
    pub created_at: u64,
}

pub struct Pty {
    tx: tokio::sync::mpsc::Sender<Bytes>,

    pub metadata: PtyMetadata,
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub reader: Arc<Mutex<Box<dyn std::io::Read + Send>>>,
    pub child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
}

#[async_trait]
impl PtyLike for Pty {
    fn metadata(&self) -> PtyMetadata {
        self.metadata.clone()
    }

    async fn kill_child(&self) -> Result<()> {
        self.kill_child().await
    }

    async fn send_bytes(&self, bytes: Bytes) -> Result<()> {
        self.send_bytes(bytes).await
    }

    async fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.resize(rows, cols).await
    }
}

impl Pty {
    pub async fn open(
        rows: u16,
        cols: u16,
        cwd: Option<String>,
        env: HashMap<String, String>,
        metadata: PtyMetadata,
        shell: Option<String>,
    ) -> Result<Self> {
        let sys = portable_pty::native_pty_system();

        let pair = sys
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| eyre!("Failed to open pty: {}", e))?;

        let mut cmd = match shell {
            Some(shell_path) if !shell_path.is_empty() => {
                let mut cmd = CommandBuilder::new(shell_path);
                cmd.arg("-i"); // Interactive mode
                cmd
            }
            _ => CommandBuilder::new_default_prog(),
        };

        // Flags to our shell integration that this is running within the desktop app
        cmd.env("ATUIN_DESKTOP_PTY", "true");
        cmd.env("TERM", "xterm-256color");

        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }

        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = match pair.slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(e) => return Err(eyre!("Failed to spawn shell process: {}", e)),
        };
        drop(pair.slave);

        // Handle input -> write to master writer
        let (master_tx, mut master_rx) = tokio::sync::mpsc::channel::<Bytes>(32);

        let mut writer = pair.master.take_writer().unwrap();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| e.to_string())
            .expect("Failed to clone reader");

        tokio::spawn(async move {
            while let Some(bytes) = master_rx.recv().await {
                writer.write_all(&bytes).unwrap();
                writer.flush().unwrap();
            }

            // When the channel has been closed, we won't be getting any more input. Close the
            // writer and the master.
            // This will also close the writer, which sends EOF to the underlying shell. Ensuring
            // that is also closed.
            drop(writer);
        });

        Ok(Pty {
            metadata,
            tx: master_tx,
            master: Arc::new(Mutex::new(pair.master)),
            reader: Arc::new(Mutex::new(reader)),
            child: Arc::new(Mutex::new(child)),
        })
    }

    pub async fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let master = self
            .master
            .lock()
            .map_err(|e| eyre!("Failed to lock pty master: {e}"))?;

        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| eyre!("Failed to resize terminal: {e}"))?;

        Ok(())
    }

    pub async fn send_bytes(&self, bytes: Bytes) -> Result<()> {
        self.tx
            .send(bytes)
            .await
            .map_err(|e| eyre!("Failed to write to master tx: {}", e))
    }

    #[allow(dead_code)]
    pub async fn send_string(&self, cmd: &str) -> Result<()> {
        let bytes: Vec<u8> = cmd.bytes().collect();
        let bytes = Bytes::from(bytes);

        self.send_bytes(bytes).await
    }

    pub async fn wait_for_shell_ready(&self) -> Result<()> {
        const REQUIRED_READY_CHECKS: i32 = 3;
        let start = Instant::now();
        let mut consecutive_ready_checks = 0;

        // Wait for shell to be ready by checking that it has no active child processes
        // When a shell is at a prompt, it typically has no children running
        let shell_pid = {
            match self.child.lock() {
                Ok(child) => match child.process_id() {
                    Some(pid) => pid,
                    None => return Err(eyre!("Unable to get shell process ID")),
                },
                Err(e) => return Err(eyre!("Failed to lock child process: {}", e)),
            }
        };

        loop {
            if start.elapsed() > Duration::from_secs(5) {
                log::warn!("Shell readiness check timed out after 5 seconds, proceeding anyway");
                return Ok(());
            }

            // Check if shell process is still running
            let is_running = {
                match self.child.lock() {
                    Ok(mut child) => {
                        match child.try_wait() {
                            Ok(Some(exit_status)) => {
                                return Err(eyre!(
                                    "Shell process exited during startup: {:?}",
                                    exit_status
                                ));
                            }
                            Ok(None) => true, // Process is still running
                            Err(e) => {
                                return Err(eyre!("Failed to check child process status: {}", e));
                            }
                        }
                    }
                    Err(e) => {
                        return Err(eyre!("Failed to lock child process: {}", e));
                    }
                }
            };

            if is_running {
                // Process is still running, check if it has children
                if self.shell_has_no_children(shell_pid).await? {
                    consecutive_ready_checks += 1;
                    if consecutive_ready_checks >= REQUIRED_READY_CHECKS {
                        log::debug!(
                            "Shell ready after {:.2}s (no child processes)",
                            start.elapsed().as_secs_f64()
                        );
                        return Ok(());
                    }
                } else {
                    // Shell is still initializing (has child processes)
                    consecutive_ready_checks = 0;
                }
            }

            // Small delay between checks
            sleep(Duration::from_millis(300)).await;
        }
    }

    async fn shell_has_no_children(&self, shell_pid: u32) -> Result<bool> {
        spawn_blocking(move || {
            let mut sys = System::new();
            sys.refresh_processes(ProcessesToUpdate::All, true);

            let shell_pid = Pid::from_u32(shell_pid);
            let mut child_pids = Vec::new();

            // Find all processes that are children of the shell
            sys.processes().iter().for_each(|(pid, process)| {
                if let Some(parent_pid) = process.parent() {
                    if parent_pid == shell_pid {
                        child_pids.push(pid.as_u32());
                    }
                }
            });

            if !child_pids.is_empty() {
                // Shell has child processes, not ready yet
                log::debug!(
                    "Shell PID {} has children: {}",
                    shell_pid.as_u32(),
                    child_pids
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                Ok(false)
            } else {
                // No child processes found, shell is ready
                log::debug!(
                    "Shell ready after {} s (no child processes)",
                    shell_pid.as_u32()
                );
                Ok(true)
            }
        })
        .await
        .map_err(|e| eyre!("Task join error: {}", e))?
    }

    #[allow(dead_code)]
    pub async fn send_single_string(&self, cmd: &str) -> Result<()> {
        let mut bytes: Vec<u8> = cmd.bytes().collect();
        bytes.push(0x04);

        let bytes = Bytes::from(bytes);

        self.send_bytes(bytes).await
    }

    pub async fn kill_child(&self) -> Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|e| eyre!("Failed to lock pty child: {e}"))?;

        child
            .kill()
            .map_err(|e| eyre!("Failed to kill child: {e}"))?;

        Ok(())
    }
}
