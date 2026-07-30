//! The sandbox abstraction: an isolated execution environment (workspace
//! filesystem + shell + network policy) behind a pluggable backend.
//!
//! This is Step 1 of `doc/sandbox.md` §7: the trait surface plus a
//! zero-isolation [`PassthroughSandbox`](passthrough::PassthroughSandbox) that
//! runs commands directly against the host (today's `ShellTool` behaviour),
//! validating the contract without committing to a real backend. Wiring the
//! shell tool onto `exec` (Step 3) and session-fork onto snapshot/restore
//! (Step 4) are later, independently-verifiable steps.
//!
//! ## Divergence from `doc/sandbox.md` §2
//!
//! The doc sketches a single `trait Sandbox` whose `create`/`restore` return
//! `Self` and whose `release` takes `self` by value. That shape is *not*
//! object-safe, yet §4.2 requires upper layers to hold the backend behind a
//! trait object so swapping backends touches one file only. We therefore split
//! the doc's single trait into two object-safe traits — [`SandboxBackend`]
//! (the factory: `create`/`restore`) and [`Sandbox`] (the live instance:
//! `exec`/`snapshot`/`release`/`capabilities`, all `&self`). The contract and
//! semantics of §2–§3 are unchanged; only the receiver shapes differ. The
//! session-fork flow of §3.2 maps directly: `parent.snapshot()` yields a
//! [`SnapshotId`], `backend.restore(id)` clones a child sandbox from it.

pub mod passthrough;

pub mod manager;

#[cfg(feature = "sandbox-boxlite")]
pub mod boxlite;

use std::sync::Arc;
use std::time::Duration;

/// A live, isolated execution environment.
///
/// Every method takes `&self` so the instance can be held as `Arc<dyn Sandbox>`
/// and shared across the session that owns it. Snapshot/fork operations assume
/// the sandbox is **quiescent** (no in-flight tool process; the shell is parked
/// at its prompt) — see `doc/sandbox.md` §3.1.
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync {
    /// Execute a command line inside the sandbox, returning its captured
    /// streams and exit status. `timeout` bounds the wall-clock budget; a
    /// backend may also enforce its own limit from [`ResourceLimits`].
    async fn exec(&self, command: &str, timeout: Duration) -> Result<ExecOutput, SandboxError>;

    /// Like [`Sandbox::exec`], but streams each output chunk (stdout and
    /// stderr interleaved as they arrive) to `on_output` as it is produced,
    /// instead of only at the end. Used by the shell tool's stage-2 output
    /// streaming (`doc/tool-streaming.md`). The default ignores `on_output`
    /// and delegates to [`Sandbox::exec`] — a backend that can't stream
    /// (`BoxLite`, TODO) silently degrades to end-only output, which the caller
    /// already tolerates (no live frames, settled view at stage 3).
    async fn exec_streaming(
        &self,
        command: &str,
        timeout: Duration,
        mut on_output: Box<dyn for<'a> FnMut(&'a [u8]) + Send>,
    ) -> Result<ExecOutput, SandboxError> {
        let _ = &mut on_output;
        self.exec(command, timeout).await
    }

    /// Capture the current filesystem state as a snapshot.
    ///
    /// **Contract**: the caller must ensure the sandbox is quiescent. The
    /// returned [`SnapshotId`] can be passed to [`SandboxBackend::restore`] to
    /// clone an equivalent sandbox. Filesystem state is the minimum guarantee;
    /// memory/process capture is an optional capability (see
    /// [`SandboxCapabilities::live_snapshot`]).
    async fn snapshot(&self) -> Result<SnapshotId, SandboxError>;

    /// Release this sandbox and its resources.
    ///
    /// **Contract**: decrements the reference count on any snapshot this
    /// sandbox was forked from; when a snapshot's count reaches zero the backend
    /// may GC it (`doc/sandbox.md` §2, §3.3). Takes `&self` (not `self`) to stay
    /// object-safe; a released sandbox must not be used again.
    async fn release(&self) -> Result<(), SandboxError>;

    /// The capabilities this sandbox instance offers.
    fn capabilities(&self) -> SandboxCapabilities;
}

/// A factory for [`Sandbox`] instances: cold-starts fresh sandboxes and
/// restores/forks them from snapshots.
///
/// Split from [`Sandbox`] because the doc's constructor-style `create`/`restore`
/// return new instances rather than acting on an existing one; keeping them off
/// the instance trait is what lets [`Sandbox`] stay object-safe.
#[async_trait::async_trait]
pub trait SandboxBackend: Send + Sync {
    /// A short, stable name for this backend (e.g. `"passthrough"`, `"boxlite"`),
    /// recorded in a session's [`SandboxDescriptor`](crate::session::SandboxDescriptor)
    /// so its environment can be re-attached after a restart.
    fn name(&self) -> &'static str;

    /// Cold-start a new sandbox from `config`'s rootfs image.
    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError>;

    /// Restore (or fork) a sandbox from a snapshot. The result has the
    /// snapshot's filesystem state and is in a ready state; for a filesystem-only
    /// backend this is a cold start, for a memory-snapshot backend it may be a
    /// warm restore. Behaviour is equivalent, only performance differs
    /// (`doc/sandbox.md` §2).
    async fn restore(&self, id: &SnapshotId) -> Result<Arc<dyn Sandbox>, SandboxError>;

    /// The capabilities every sandbox this backend produces will report.
    fn capabilities(&self) -> SandboxCapabilities;
}

/// Fork a sandbox: snapshot `parent`, then restore that snapshot on `backend`
/// into a fresh, independently-writable child (`doc/sandbox.md` §3.2, Step 4).
///
/// This is the mechanism a session fork uses to hand a branched session a
/// copy-on-write child of its parent's filesystem, rather than a second handle
/// to the same directory.
///
/// # Errors
/// Fails loud with [`SandboxError::Unsupported`] when `parent` cannot snapshot
/// (its [`SandboxCapabilities::filesystem_snapshot`] is `false`, e.g. the
/// passthrough backend, which shares the host workspace). The caller must then
/// decide whether to refuse the fork or fall back to a shared workspace — it
/// must never silently let two sessions write one directory. Snapshot or restore
/// faults surface as [`SandboxError::Snapshot`].
pub async fn fork_sandbox(
    backend: &dyn SandboxBackend,
    parent: &Arc<dyn Sandbox>,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    if !parent.capabilities().filesystem_snapshot {
        return Err(SandboxError::Unsupported(
            "sandbox fork requires filesystem_snapshot; this backend cannot snapshot",
        ));
    }
    let id = parent.snapshot().await?;
    backend.restore(&id).await
}

/// Configuration for cold-starting a sandbox (`doc/sandbox.md` §2).
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    /// Workspace directory bound as the sandbox's working directory. This is the
    /// primary — and currently only — mount (`doc/sandbox.md` §3.3), realized as
    /// `cwd` on every backend so relative paths behave identically. Auxiliary
    /// mounts (private tmp / delivery / shared) are a future feature (§3.7).
    pub workspace: std::path::PathBuf,
    /// Environment overlay applied to every command (workspace `direnv`/dotenv
    /// activation). Keys mapping to `None` are unset.
    pub env: std::collections::BTreeMap<String, Option<String>>,
    /// Rootfs image (OCI ref or local path). Ignored by the passthrough
    /// backend, which runs against the host filesystem.
    pub rootfs: String,
    /// Resource limits (time / memory / CPU).
    pub resources: ResourceLimits,
    /// Network policy.
    pub network: NetworkPolicy,
    /// Initial mounts used to seed the workspace.
    pub volumes: Vec<VolumeMount>,
}

/// Resource limits applied to a sandbox (`doc/sandbox.md` §5.2).
///
/// `None` means "backend default / unlimited"; the passthrough backend enforces
/// none of these except the per-`exec` timeout its caller passes.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Default wall-clock budget for a command when the caller does not
    /// override it. §5.2 sets the default at 120s.
    pub timeout: Duration,
    /// Memory cap in megabytes.
    pub memory_mb: Option<u64>,
    /// CPU core budget.
    pub cpus: Option<u32>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            memory_mb: None,
            cpus: None,
        }
    }
}

/// Network egress policy for a sandbox (`doc/sandbox.md` §5.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No network (microVM gets no NIC).
    #[default]
    Isolated,
    /// Only the listed hosts are reachable (DNS sinkhole blocks the rest).
    AllowList(Vec<String>),
    /// Unrestricted egress.
    Open,
}

impl NetworkPolicy {
    /// Resolve a config-facing policy name (`isolated` | `allowlist` | `open`)
    /// plus its allow-list into a concrete policy. Shared by the profile
    /// `[network]` section and the gateway default (`doc/sandbox.md` §6.2).
    ///
    /// `allow` is only consumed by `allowlist`; it is ignored for the others.
    ///
    /// # Errors
    /// An unrecognized name is a hard error — a typo must never silently fall
    /// back to open (leak) or isolated (break every command). Fail loud
    /// (Karpathy §12) so the misconfiguration surfaces at session start.
    pub fn from_policy_name(name: &str, allow: &[String]) -> Result<Self, String> {
        match name {
            "isolated" => Ok(Self::Isolated),
            "allowlist" => Ok(Self::AllowList(allow.to_vec())),
            "open" => Ok(Self::Open),
            other => Err(format!(
                "unknown network policy `{other}`; expected `isolated`, `allowlist`, or `open`"
            )),
        }
    }
}

/// An initial mount injected into the sandbox at create time.
#[derive(Debug, Clone)]
pub struct VolumeMount {
    /// Path on the host.
    pub host_path: std::path::PathBuf,
    /// Mount point inside the sandbox.
    pub guest_path: std::path::PathBuf,
    /// Whether the guest may write through this mount.
    pub read_only: bool,
}

/// An opaque handle to a captured snapshot, returned by [`Sandbox::snapshot`]
/// and consumed by [`SandboxBackend::restore`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotId(pub String);

/// Optional-capability flags a backend advertises (`doc/sandbox.md` §2).
///
/// The minimum contract is a filesystem snapshot in the quiescent state; every
/// other flag is a bonus a specific backend may or may not offer. Upper layers
/// query these before relying on a capability rather than assuming it.
///
/// The four flags are independent feature bits mirroring §2's capability set,
/// not a state that an enum would model better — hence the `struct_excessive_bools`
/// allow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SandboxCapabilities {
    /// Backend can capture filesystem state as a snapshot at all. The
    /// passthrough backend reports `false` — it shares the host directory and
    /// has nothing to snapshot.
    pub filesystem_snapshot: bool,
    /// Backend can capture live memory/process state, not just the filesystem.
    pub live_snapshot: bool,
    /// Backend can fork from a snapshot in ~seconds (warm) rather than cold-start.
    pub hot_fork: bool,
    /// Backend GCs snapshot chains automatically via reference counting.
    pub refcounted_gc: bool,
}

/// The captured result of a [`Sandbox::exec`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code, or `None` if the process was killed by a signal.
    pub exit_code: Option<i32>,
}

impl ExecOutput {
    /// Whether the command exited successfully (code 0).
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }
}

/// A protocol-level sandbox failure.
///
/// Mirrors [`crate::tool::ToolError`]'s split: a command exiting non-zero is a
/// *successful* [`exec`](Sandbox::exec) with a non-zero [`ExecOutput::exit_code`],
/// not a `SandboxError`. This enum is reserved for the sandbox itself failing to
/// do its job — a backend that cannot spawn, a timeout, an unsupported
/// operation.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The backend does not support this operation (e.g. the passthrough
    /// backend cannot snapshot). The `&'static str` names the operation.
    #[error("sandbox operation unsupported: {0}")]
    Unsupported(&'static str),

    /// The command could not be run (failed to spawn, backend fault).
    #[error("sandbox exec error: {0}")]
    Exec(String),

    /// The command exceeded its time budget.
    #[error("sandbox exec timed out after {0:?}")]
    Timeout(Duration),

    /// A snapshot or restore operation failed.
    #[error("sandbox snapshot error: {0}")]
    Snapshot(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::sandbox::passthrough::PassthroughBackend;

    /// A backend/sandbox pair that supports snapshot+restore, so `fork_sandbox`
    /// can be exercised without a real microVM. `snapshot` hands back a fixed id;
    /// `restore` yields a fresh capable sandbox and records the id it saw.
    #[derive(Clone)]
    struct CapableBackend {
        restored_from: Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct CapableSandbox;

    const CAPABLE: SandboxCapabilities = SandboxCapabilities {
        filesystem_snapshot: true,
        live_snapshot: false,
        hot_fork: false,
        refcounted_gc: false,
    };

    #[async_trait::async_trait]
    impl Sandbox for CapableSandbox {
        async fn exec(&self, _c: &str, _t: Duration) -> Result<ExecOutput, SandboxError> {
            unreachable!("fork does not exec")
        }
        async fn snapshot(&self) -> Result<SnapshotId, SandboxError> {
            Ok(SnapshotId("snap-1".to_owned()))
        }
        async fn release(&self) -> Result<(), SandboxError> {
            Ok(())
        }
        fn capabilities(&self) -> SandboxCapabilities {
            CAPABLE
        }
    }

    #[async_trait::async_trait]
    impl SandboxBackend for CapableBackend {
        fn name(&self) -> &'static str {
            "capable-fake"
        }

        async fn create(&self, _c: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
            Ok(Arc::new(CapableSandbox))
        }
        async fn restore(&self, id: &SnapshotId) -> Result<Arc<dyn Sandbox>, SandboxError> {
            self.restored_from.lock().unwrap().push(id.0.clone());
            Ok(Arc::new(CapableSandbox))
        }
        fn capabilities(&self) -> SandboxCapabilities {
            CAPABLE
        }
    }

    #[tokio::test]
    async fn fork_snapshots_parent_and_restores_onto_backend() {
        let backend = CapableBackend {
            restored_from: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let parent: Arc<dyn Sandbox> = Arc::new(CapableSandbox);

        let child = fork_sandbox(&backend, &parent).await.unwrap();
        assert!(child.capabilities().filesystem_snapshot);
        // restore was fed exactly the id snapshot produced — the fork wiring
        // threads the parent's snapshot into the child, not some other id.
        assert_eq!(backend.restored_from.lock().unwrap().as_slice(), ["snap-1"]);
    }

    #[tokio::test]
    async fn fork_is_unsupported_when_parent_cannot_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let backend = PassthroughBackend::new();
        let cfg = SandboxConfig {
            workspace: dir.path().to_path_buf(),
            ..SandboxConfig::default()
        };
        let parent = backend.create(cfg).await.unwrap();

        let result = fork_sandbox(&backend, &parent).await;
        assert!(matches!(result, Err(SandboxError::Unsupported(_))));
    }

    #[test]
    fn network_policy_maps_names_and_rejects_typos() {
        let hosts = vec!["crates.io".to_owned()];
        assert_eq!(
            NetworkPolicy::from_policy_name("isolated", &[]).unwrap(),
            NetworkPolicy::Isolated
        );
        assert_eq!(
            NetworkPolicy::from_policy_name("open", &[]).unwrap(),
            NetworkPolicy::Open
        );
        // allowlist carries the hosts through; other policies ignore them.
        assert_eq!(
            NetworkPolicy::from_policy_name("allowlist", &hosts).unwrap(),
            NetworkPolicy::AllowList(hosts.clone())
        );
        // A typo must fail loud, not silently open or isolate the network.
        assert!(NetworkPolicy::from_policy_name("opne", &hosts).is_err());
    }
}
