//! [`PassthroughSandbox`]: the zero-isolation backend (`doc/design/runtime-architecture.md` §7,
//! Step 1).
//!
//! It runs commands directly on the host via `sh -c` with the workspace as the
//! working directory — the same execution path `ShellTool` uses today — so the
//! [`Sandbox`] contract can be exercised before a real isolating backend
//! (`BoxLite`, Step 2) exists. Because it shares the host directory and provides
//! no isolation, it *cannot* honour the filesystem-snapshot contract of §2:
//! [`snapshot`](PassthroughSandbox::snapshot) and
//! [`SandboxBackend::restore`] return [`SandboxError::Unsupported`] rather than
//! silently handing back a shared, unisolated directory (fail loud — a session
//! fork on this backend must error, not corrupt one workspace from two
//! sessions).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use super::{
    ExecOutput, Sandbox, SandboxBackend, SandboxCapabilities, SandboxConfig, SandboxError,
    SnapshotId,
};
use crate::process_env::apply_env_overlay;

/// A [`SandboxBackend`] that produces [`PassthroughSandbox`] instances.
///
/// It is stateless: each sandbox's workspace and environment come from the
/// [`SandboxConfig`] passed to [`create`](PassthroughBackend::create), so one
/// backend serves every session in the process.
#[derive(Debug, Clone, Default)]
pub struct PassthroughBackend;

impl PassthroughBackend {
    /// Create a passthrough backend.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl SandboxBackend for PassthroughBackend {
    fn name(&self) -> &'static str {
        "passthrough"
    }

    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
        // The passthrough backend runs on the host: it honours `workspace` (as
        // cwd) and `env`, and ignores rootfs/resources/network (no isolation).
        // Auxiliary mounts (§3.7) it *cannot* honour — with no namespace there is
        // no way to bind a host dir onto an arbitrary guest absolute path. Fail
        // loud rather than silently drop them: a session that declared a mount and
        // silently didn't get it is worse than one that refused to start.
        if !config.volumes.is_empty() {
            return Err(SandboxError::Unsupported(
                "passthrough backend cannot honour auxiliary mounts (no namespace); \
                 use the boxlite backend for [[mounts]]",
            ));
        }
        Ok(Arc::new(PassthroughSandbox::new(
            config.workspace,
            config.env,
        )))
    }

    async fn restore(&self, _id: &SnapshotId) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Err(SandboxError::Unsupported(
            "passthrough backend cannot restore: it shares the host workspace and has no snapshots",
        ))
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::default()
    }
}

/// A sandbox that executes directly on the host with no isolation.
#[derive(Debug, Clone)]
pub struct PassthroughSandbox {
    workspace: PathBuf,
    env_overlay: BTreeMap<String, Option<String>>,
}

impl PassthroughSandbox {
    /// Create a passthrough sandbox rooted at `workspace`.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(workspace: PathBuf, env_overlay: BTreeMap<String, Option<String>>) -> Self {
        Self {
            workspace,
            env_overlay,
        }
    }
}

/// Spawn a task forwarding each chunk read from `pipe` as `(is_err, chunk)`.
/// The task ends on EOF, error, or a closed receiver.
fn spawn_pipe_reader<R>(
    mut pipe: R,
    is_err: bool,
    tx: tokio::sync::mpsc::UnboundedSender<(bool, Vec<u8>)>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    if tx.send((is_err, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                // EOF (Ok(0)) or a read error ends the reader.
                _ => break,
            }
        }
    });
}

#[async_trait::async_trait]
impl Sandbox for PassthroughSandbox {
    async fn exec(&self, command: &str, timeout: Duration) -> Result<ExecOutput, SandboxError> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(&self.workspace);
        apply_env_overlay(&mut cmd, &self.env_overlay);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::Exec(format!("failed to spawn shell: {e}")))?;

        // Both pipes were set to `piped()` above, so they are always present.
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(SandboxError::Exec(
                "failed to capture shell pipes".to_owned(),
            ));
        };

        // Read both pipes concurrently, forwarding each chunk live and
        // accumulating for the final ExecOutput. The whole read is bounded by
        // the same wall-clock timeout; on expiry the child is killed.
        let run = async {
            use tokio::sync::mpsc;
            // Each pipe gets a reader task forwarding `(is_stderr, chunk)`; the
            // main loop merges them in arrival order and accumulates per-stream.
            let (tx, mut rx) = mpsc::unbounded_channel::<(bool, Vec<u8>)>();
            spawn_pipe_reader(stdout, false, tx.clone());
            spawn_pipe_reader(stderr, true, tx.clone());
            drop(tx); // rx ends when both reader tasks finish

            let mut out_buf: Vec<u8> = Vec::new();
            let mut err_buf: Vec<u8> = Vec::new();
            while let Some((is_err, chunk)) = rx.recv().await {
                if is_err {
                    err_buf.extend_from_slice(&chunk);
                } else {
                    out_buf.extend_from_slice(&chunk);
                }
            }
            let status = child
                .wait()
                .await
                .map_err(|e| SandboxError::Exec(e.to_string()))?;
            Ok((out_buf, err_buf, status))
        };

        #[allow(clippy::single_match_else)] // the Err arm returns, the Ok arm unwraps
        let (out_buf, err_buf, status) = match tokio::time::timeout(timeout, run).await {
            Ok(res) => res?,
            Err(_) => {
                let _ = child.kill().await;
                return Err(SandboxError::Timeout(timeout));
            }
        };

        Ok(ExecOutput {
            stdout: String::from_utf8_lossy(&out_buf).into_owned(),
            stderr: String::from_utf8_lossy(&err_buf).into_owned(),
            exit_code: status.code(),
        })
    }

    async fn snapshot(&self) -> Result<SnapshotId, SandboxError> {
        Err(SandboxError::Unsupported(
            "passthrough backend shares the host workspace and cannot snapshot it",
        ))
    }

    async fn release(&self) -> Result<(), SandboxError> {
        // Nothing to release: the host workspace outlives the sandbox and is not
        // owned by it.
        Ok(())
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sandbox(workspace: PathBuf) -> PassthroughSandbox {
        PassthroughSandbox::new(workspace, BTreeMap::new())
    }

    #[tokio::test]
    async fn exec_captures_stdout_and_zero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path().to_path_buf());

        let out = sb.exec("echo hello", Duration::from_secs(5)).await.unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "hello\n");
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn exec_reports_nonzero_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path().to_path_buf());

        let out = sb.exec("exit 3", Duration::from_secs(5)).await.unwrap();
        assert!(!out.success());
        assert_eq!(out.exit_code, Some(3));
    }

    #[tokio::test]
    async fn exec_runs_in_workspace_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let sb = sandbox(dir.path().to_path_buf());

        let out = sb.exec("ls", Duration::from_secs(5)).await.unwrap();
        assert!(out.stdout.contains("marker.txt"));
    }

    #[tokio::test]
    async fn exec_applies_env_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let sb = PassthroughSandbox::new(
            dir.path().to_path_buf(),
            BTreeMap::from([("OMINI_SANDBOX_TEST".to_owned(), Some("active".to_owned()))]),
        );

        let out = sb
            .exec("printf %s \"$OMINI_SANDBOX_TEST\"", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "active");
    }

    #[tokio::test]
    async fn exec_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path().to_path_buf());

        let result = sb.exec("sleep 5", Duration::from_millis(50)).await;
        assert!(matches!(result, Err(SandboxError::Timeout(_))));
    }

    #[tokio::test]
    async fn snapshot_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path().to_path_buf());

        assert!(matches!(
            sb.snapshot().await,
            Err(SandboxError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn restore_is_unsupported() {
        let backend = PassthroughBackend::new();

        let result = backend.restore(&SnapshotId("nonexistent".to_owned())).await;
        assert!(matches!(result, Err(SandboxError::Unsupported(_))));
    }

    #[tokio::test]
    async fn capabilities_are_all_false() {
        let dir = tempfile::tempdir().unwrap();
        let sb = sandbox(dir.path().to_path_buf());

        let caps = sb.capabilities();
        assert_eq!(caps, SandboxCapabilities::default());
        assert!(!caps.filesystem_snapshot);
        assert!(!caps.live_snapshot);
        assert!(!caps.hot_fork);
        assert!(!caps.refcounted_gc);
    }

    #[tokio::test]
    async fn backend_create_yields_working_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let backend = PassthroughBackend::new();

        let cfg = SandboxConfig {
            workspace: dir.path().to_path_buf(),
            ..SandboxConfig::default()
        };
        let sb = backend.create(cfg).await.unwrap();
        let out = sb.exec("echo ok", Duration::from_secs(5)).await.unwrap();
        assert_eq!(out.stdout, "ok\n");
        assert!(sb.release().await.is_ok());
    }

    /// Passthrough cannot honour auxiliary mounts (no namespace), so a config
    /// carrying `[[mounts]]` must fail loud, not silently drop them (§3.7). A
    /// session that declared a mount and silently didn't get it is the danger.
    #[tokio::test]
    async fn backend_create_rejects_non_empty_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let backend = PassthroughBackend::new();
        let cfg = SandboxConfig {
            workspace: dir.path().to_path_buf(),
            volumes: vec![crate::sandbox::VolumeMount {
                host_path: dir.path().to_path_buf(),
                guest_path: PathBuf::from("/cache"),
                read_only: false,
            }],
            ..SandboxConfig::default()
        };
        assert!(matches!(
            backend.create(cfg).await,
            Err(SandboxError::Unsupported(_))
        ));
    }
}
