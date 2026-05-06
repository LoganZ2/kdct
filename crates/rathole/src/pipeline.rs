//! Pipeline executor — runs shell commands sequentially, streams output back.
//!
//! Platform support:
//! - Unix (Linux/macOS): `sh -c <command>`
//! - Windows: `cmd /C <command>`

use crate::protocol::{ControlChannelCmd, PipelineId, PipelineStep};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{self, Duration};
use tracing::{debug, info, warn};

/// Map of active pipeline IDs → cancellation sender.
/// When a CancelPipeline command arrives, the sender is dropped,
/// which signals the executing task to stop.
type CancelMap = Arc<RwLock<HashMap<PipelineId, tokio::sync::watch::Sender<bool>>>>;

lazy_static::lazy_static! {
    static ref CANCEL_REGISTRY: CancelMap = Arc::new(RwLock::new(HashMap::new()));
}

/// Signal cancellation for a running pipeline.
pub async fn cancel(id: &PipelineId) {
    let guard = CANCEL_REGISTRY.read().await;
    if let Some(tx) = guard.get(id) {
        let _ = tx.send(true);
        info!("Cancelled pipeline: {}", id);
    }
}

/// Clean up the cancellation entry for a completed pipeline.
async fn unregister(id: &PipelineId) {
    CANCEL_REGISTRY.write().await.remove(id);
}

/// Execute a pipeline: steps run sequentially, output is streamed via `tx`.
/// Stops on first step failure. Respects cancellation.
pub async fn execute(id: PipelineId, steps: Vec<PipelineStep>, tx: mpsc::Sender<ControlChannelCmd>) {
    info!("Starting pipeline {} with {} steps", id, steps.len());

    // Register for cancellation
    let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
    CANCEL_REGISTRY.write().await.insert(id.clone(), cancel_tx);

    let result = execute_steps(&id, &steps, &tx, &mut cancel_rx).await;

    unregister(&id).await;

    match result {
        Ok(()) => info!("Pipeline {} completed successfully", id),
        Err(e) => warn!("Pipeline {} failed: {:#}", id, e),
    }
}

async fn execute_steps(
    id: &PipelineId,
    steps: &[PipelineStep],
    tx: &mpsc::Sender<ControlChannelCmd>,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    for step in steps {
        // Check cancellation before each step
        if *cancel_rx.borrow() {
            let _ = tx
                .send(ControlChannelCmd::PipelineOutput {
                    id: id.clone(),
                    step: step.name.clone(),
                    stdout: vec![],
                    stderr: b"Pipeline cancelled".to_vec(),
                    exit_code: Some(-1),
                })
                .await;
            bail!("Pipeline {} cancelled at step {}", id, step.name);
        }

        run_step(id, step, tx, cancel_rx).await?;
    }
    Ok(())
}

async fn run_step(
    id: &PipelineId,
    step: &PipelineStep,
    tx: &mpsc::Sender<ControlChannelCmd>,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    info!("Pipeline {} — step: {}", id, step.name);

    // Platform shell selection
    let (shell, flag) = if cfg!(target_os = "windows") {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };

    let mut child = Command::new(shell)
        .arg(flag)
        .arg(&step.command)
        .current_dir(step.cwd.as_deref().unwrap_or("."))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Failed to spawn command for step: {}", step.name))?;

    let stdout = child
        .stdout
        .take()
        .context("Failed to capture stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("Failed to capture stderr")?;

    let (stdout_tx, mut stdout_rx) = mpsc::channel::<Vec<u8>>(64);
    let (stderr_tx, mut stderr_rx) = mpsc::channel::<Vec<u8>>(64);

    // Spawn readers for stdout/stderr
    tokio::spawn(read_lines(stdout, stdout_tx));
    tokio::spawn(read_lines(stderr, stderr_tx));

    let step_name = step.name.clone();
    let pipeline_id = id.clone();
    let output_tx = tx.clone();

    // Main event loop: stream output + wait for child exit
    let exit_code = 'step: loop {
        // Check cancellation
        if *cancel_rx.borrow() {
            child.kill().await.ok();
            return Err(anyhow::anyhow!("Cancelled"));
        }

        tokio::select! {
            biased;

            // Check cancellation (polled frequently)
            _ = cancel_rx.changed() => {
                child.kill().await.ok();
                return Err(anyhow::anyhow!("Cancelled"));
            }

            // Stdout chunk
            data = stdout_rx.recv() => {
                if let Some(data) = data {
                    let _ = output_tx.send(ControlChannelCmd::PipelineOutput {
                        id: pipeline_id.clone(),
                        step: step_name.clone(),
                        stdout: data,
                        stderr: vec![],
                        exit_code: None,
                    }).await;
                }
            }

            // Stderr chunk
            data = stderr_rx.recv() => {
                if let Some(data) = data {
                    let _ = output_tx.send(ControlChannelCmd::PipelineOutput {
                        id: pipeline_id.clone(),
                        step: step_name.clone(),
                        stdout: vec![],
                        stderr: data,
                        exit_code: None,
                    }).await;
                }
            }

            // Child process exited
            status = child.wait() => {
                let code = status
                    .map(|s| s.code())
                    .unwrap_or_default();
                debug!("Pipeline {} step {} exited with {:?}", pipeline_id, step_name, code);

                // Send final output for this step with exit code
                let _ = output_tx.send(ControlChannelCmd::PipelineOutput {
                    id: pipeline_id.clone(),
                    step: step_name.clone(),
                    stdout: vec![],
                    stderr: vec![],
                    exit_code: code,
                }).await;

                break 'step code;
            }

            // Timeout (if set)
            _ = time::sleep(Duration::from_secs(step.timeout_secs)), if step.timeout_secs > 0 => {
                child.kill().await.ok();
                let _ = output_tx.send(ControlChannelCmd::PipelineOutput {
                    id: pipeline_id.clone(),
                    step: step_name.clone(),
                    stdout: vec![],
                    stderr: b"Step timed out".to_vec(),
                    exit_code: Some(-1),
                }).await;
                return Err(anyhow::anyhow!("Step {} timed out after {}s", step_name, step.timeout_secs));
            }
        }
    };

    // Check result — stop pipeline on non-zero exit
    match exit_code {
        Some(0) | None => Ok(()),
        Some(code) => {
            Err(anyhow::anyhow!("Step {} failed with exit code {}", step_name, code))
        }
    }
}

/// Read lines from a stream and send them in chunks over a channel.
async fn read_lines<R: tokio::io::AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<Vec<u8>>) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut data = line.into_bytes();
        data.push(b'\n');
        if tx.send(data).await.is_err() {
            break; // Receiver dropped
        }
    }
}
