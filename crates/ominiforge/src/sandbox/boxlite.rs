//! [`BoxliteSandbox`]: the `BoxLite` (libkrun microVM) backend
//! (`doc/sandbox.md` §4, Step 2).
//!
//! This is the real isolating backend. Every `boxlite` dependency lives in this
//! one file, behind [`Sandbox`]/[`SandboxBackend`] — if `BoxLite` churns or is
//! replaced, only this file changes (§4.2). The whole module is gated behind the
//! `sandbox-boxlite` cargo feature because libkrun targets Linux + Apple Silicon
//! only and pulls a heavy vendored native stack (§8); a default build ships only
//! [`super::passthrough`].
//!
//! ## How the abstraction maps onto `BoxLite`'s real API
//!
//! The names in §4.1 are indicative; the shapes below were verified against
//! `boxlite` 0.9.7 source and diverge from the doc sketch in three load-bearing
//! ways, all handled here:
//!
//! 1. **Fork = clone, not snapshot-restore.** `BoxLite`'s `SnapshotHandle::restore`
//!    restores disks *in place* on the same box; it does not yield a new box.
//!    The primitive that yields an independent child with a shared base disk +
//!    thin `CoW` overlay (~the §3.3 property `BoxLite` was chosen for) is
//!    `LiteBox::clone_box`. So [`snapshot`](BoxliteSandbox::snapshot) clones an
//!    immutable *template* box and returns its id; [`SandboxBackend::restore`]
//!    fetches that template and clones a fresh child from it.
//! 2. **Boxes must not auto-remove.** `BoxLite` defaults to Docker `--rm`
//!    semantics; a template that auto-removed on stop would vanish. Boxes we
//!    create set `auto_remove = false`, and [`release`](BoxliteSandbox::release)
//!    tears the box down explicitly via `runtime.remove(force)` — which is also
//!    what decrements `BoxLite`'s base-disk refcount and lets its GC reclaim the
//!    chain (§3.3).
//! 3. **Exec output is streamed, not returned.** `Execution` exposes stdout and
//!    stderr as `Stream<Item = String>` and `wait()` yields only the exit code.
//!    [`exec`](BoxliteSandbox::exec) drains both streams concurrently with the
//!    wait and reassembles them into [`ExecOutput`].

use std::sync::Arc;
use std::time::Duration;

use boxlite::runtime::options::VolumeSpec;
use boxlite::{
    AdvancedBoxOptions, BoxCommand, BoxOptions, BoxliteRuntime, CloneOptions, LiteBox, NetworkSpec,
    RootfsSpec, SecurityOptions,
};
use futures_util::{Stream, StreamExt};

use super::{
    ExecOutput, NetworkPolicy, Sandbox, SandboxBackend, SandboxCapabilities, SandboxConfig,
    SandboxError, SnapshotId,
};

/// Capabilities every `BoxLite` sandbox reports.
///
/// `filesystem_snapshot`/`refcounted_gc` are genuine (qcow2 `CoW` + base-disk
/// refcount GC, §4.1). `hot_fork` is `false` conservatively: cloning a box's
/// disks is thin-CoW cheap, but the child still cold-boots a VM, so we do not
/// advertise sub-second warm forks. `live_snapshot` is `false` — `BoxLite`
/// captures filesystem state only, not live VM memory (§3.1).
const CAPABILITIES: SandboxCapabilities = SandboxCapabilities {
    filesystem_snapshot: true,
    live_snapshot: false,
    hot_fork: false,
    refcounted_gc: true,
};

/// A [`SandboxBackend`] over a `BoxLite` runtime.
///
/// The runtime is cheap to clone (`Arc`-backed) and shared by every sandbox it
/// creates, so `create`/`restore` and each sandbox's `release` all act on one
/// runtime.
#[derive(Clone)]
pub struct BoxliteBackend {
    runtime: BoxliteRuntime,
}

impl BoxliteBackend {
    /// Create a backend over a `BoxLite` runtime with default options (home dir
    /// `~/.boxlite`).
    ///
    /// # Errors
    /// [`SandboxError::Exec`] if the runtime cannot initialize (e.g. another
    /// runtime already holds the home-directory lock, or the host lacks KVM).
    pub fn new() -> Result<Self, SandboxError> {
        let runtime = BoxliteRuntime::with_defaults()
            .map_err(|e| SandboxError::Exec(format!("boxlite runtime init failed: {e}")))?;
        Ok(Self { runtime })
    }

    /// Wrap an already-constructed runtime (lets a caller share one runtime /
    /// point it at a custom home dir).
    #[must_use]
    pub const fn with_runtime(runtime: BoxliteRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait]
impl SandboxBackend for BoxliteBackend {
    fn name(&self) -> &'static str {
        "boxlite"
    }

    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let litebox = self
            .runtime
            .create(box_options(&config), None)
            .await
            .map_err(|e| SandboxError::Exec(format!("boxlite create failed: {e}")))?;
        Ok(Arc::new(BoxliteSandbox {
            runtime: self.runtime.clone(),
            litebox,
        }))
    }

    async fn restore(&self, id: &SnapshotId) -> Result<Arc<dyn Sandbox>, SandboxError> {
        // The SnapshotId is the template box's id (see BoxliteSandbox::snapshot).
        let template = self
            .runtime
            .get(&id.0)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("boxlite get template failed: {e}")))?
            .ok_or_else(|| {
                SandboxError::Snapshot(format!("snapshot template box not found: {}", id.0))
            })?;
        // Clone a fresh, independently-writable child (shared base disk + thin
        // CoW overlay). This is the §3.2 fork.
        let child = template
            .clone_box(CloneOptions::default(), None)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("boxlite clone failed: {e}")))?;
        Ok(Arc::new(BoxliteSandbox {
            runtime: self.runtime.clone(),
            litebox: child,
        }))
    }

    fn capabilities(&self) -> SandboxCapabilities {
        CAPABILITIES
    }
}

/// A single `BoxLite` microVM sandbox.
pub struct BoxliteSandbox {
    runtime: BoxliteRuntime,
    litebox: LiteBox,
}

#[async_trait::async_trait]
impl Sandbox for BoxliteSandbox {
    async fn exec(&self, command: &str, timeout: Duration) -> Result<ExecOutput, SandboxError> {
        // Run through the guest shell, mirroring the passthrough backend's
        // `sh -c <command>`. The in-guest timeout tells BoxLite to reap the
        // process; the host-side timeout below is the §5.2 fallback.
        let cmd = BoxCommand::new("sh")
            .arg("-c")
            .arg(command.to_owned())
            .timeout(timeout);

        let mut execution = self
            .litebox
            .exec(cmd)
            .await
            .map_err(|e| SandboxError::Exec(format!("boxlite exec failed: {e}")))?;

        let stdout = execution.stdout();
        let stderr = execution.stderr();

        let drive = async { tokio::join!(drain(stdout), drain(stderr), execution.wait()) };

        match tokio::time::timeout(timeout, drive).await {
            Ok((stdout, stderr, result)) => {
                let result =
                    result.map_err(|e| SandboxError::Exec(format!("boxlite wait failed: {e}")))?;
                Ok(ExecOutput {
                    stdout,
                    stderr,
                    exit_code: normalize_exit_code(result.exit_code),
                })
            }
            Err(_) => Err(SandboxError::Timeout(timeout)),
        }
    }

    async fn snapshot(&self) -> Result<SnapshotId, SandboxError> {
        // Fork-as-clone (see module docs): clone an immutable template box whose
        // disks persist (auto_remove is false on boxes we create). `restore`
        // later clones children from it. The template is not started, so no VM
        // runs until a child execs.
        let template = self
            .litebox
            .clone_box(CloneOptions::default(), None)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("boxlite snapshot-clone failed: {e}")))?;
        Ok(SnapshotId(template.id().to_string()))
    }

    async fn release(&self) -> Result<(), SandboxError> {
        // Remove the box and its disks, which decrements BoxLite's base-disk
        // refcount and lets its GC reclaim any now-orphaned snapshot chain (§3.3).
        self.runtime
            .remove(self.litebox.id().as_ref(), true)
            .await
            .map_err(|e| SandboxError::Exec(format!("boxlite release failed: {e}")))
    }

    fn capabilities(&self) -> SandboxCapabilities {
        CAPABILITIES
    }
}

/// Drain a guest output stream to a `String`.
///
/// `BoxLite` yields output as a stream of `String` chunks; the sender frames them
/// per line without the trailing newline, so we re-append `\n` per chunk to
/// reconstruct the text (matching the passthrough backend, where `echo hello`
/// yields `"hello\n"`). The exact chunk framing is confirmed by the `#[ignore]`
/// integration test — this is the one spot that needs a real guest to pin down.
async fn drain<S>(stream: Option<S>) -> String
where
    S: Stream<Item = String> + Unpin,
{
    let mut buf = String::new();
    if let Some(mut stream) = stream {
        while let Some(chunk) = stream.next().await {
            buf.push_str(&chunk);
            buf.push('\n');
        }
    }
    buf
}

/// Map a `BoxLite` exit code onto [`ExecOutput::exit_code`].
///
/// `BoxLite` reports a negative number when the process was killed by a signal;
/// the passthrough backend reports `None` in that case (`status.code()` is
/// `None` on signal death). Normalize to that shared convention so
/// [`ExecOutput::success`] and downstream error handling behave identically
/// across backends.
const fn normalize_exit_code(code: i32) -> Option<i32> {
    if code < 0 { None } else { Some(code) }
}

/// Guest mount point for the session workspace (`doc/sandbox.md` §3.3). A fixed,
/// portable absolute path so `cwd` behaves identically to the passthrough
/// backend (where cwd *is* the host workspace); the host path itself does not
/// exist inside the guest, so it cannot be reused verbatim.
const WORKSPACE_GUEST_PATH: &str = "/workspace";

/// Translate a [`SandboxConfig`] into `BoxLite` [`BoxOptions`].
///
/// `auto_remove`/`detach` are forced off so boxes (and cloned templates) persist
/// until an explicit [`Sandbox::release`]; see the module docs.
///
/// The workspace (§3.3) is realized as a read-write FUSE bind mount at
/// [`WORKSPACE_GUEST_PATH`] plus `working_dir`, so a guest shell's `pwd` lands in
/// the project — matching the passthrough backend's cwd. This is a live
/// passthrough of the host directory, *not* part of the box's `CoW` disk: a
/// forked child (`clone_box` reuses the parent's `BoxOptions`, so it inherits
/// this mount) writes through to the same host workspace. That is the §3.3
/// contract — the workspace is the user's external path, forked isolation covers
/// the sandbox's own `CoW` filesystem (§4.2), and code merges go through git.
fn box_options(config: &SandboxConfig) -> BoxOptions {
    let rootfs = if config.rootfs.is_empty() {
        RootfsSpec::default()
    } else {
        RootfsSpec::Image(config.rootfs.clone())
    };

    let network = match &config.network {
        NetworkPolicy::Isolated => NetworkSpec::Disabled,
        NetworkPolicy::Open => NetworkSpec::Enabled {
            allow_net: Vec::new(),
        },
        NetworkPolicy::AllowList(hosts) => NetworkSpec::Enabled {
            allow_net: hosts.clone(),
        },
    };

    // Explicit initial mounts (§3.7 future), then the workspace mount if set.
    let mut volumes: Vec<VolumeSpec> = config
        .volumes
        .iter()
        .map(|v| VolumeSpec {
            host_path: v.host_path.to_string_lossy().into_owned(),
            guest_path: v.guest_path.to_string_lossy().into_owned(),
            read_only: v.read_only,
        })
        .collect();

    // An empty workspace means "no workspace bound" (e.g. a default config); do
    // not mount `/` and do not pin cwd, leaving the image's own working_dir.
    let working_dir = if config.workspace.as_os_str().is_empty() {
        None
    } else {
        volumes.push(VolumeSpec {
            host_path: config.workspace.to_string_lossy().into_owned(),
            guest_path: WORKSPACE_GUEST_PATH.to_owned(),
            read_only: false,
        });
        Some(WORKSPACE_GUEST_PATH.to_owned())
    };

    BoxOptions {
        working_dir,
        // u32 cores -> u8, saturating: BoxLite caps far below 255 in practice.
        cpus: config
            .resources
            .cpus
            .map(|c| u8::try_from(c).unwrap_or(u8::MAX)),
        // memory_mb (MB) -> memory_mib (MiB): near-identical units; the small
        // difference is immaterial for a resource ceiling.
        memory_mib: config
            .resources
            .memory_mb
            .map(|m| u32::try_from(m).unwrap_or(u32::MAX)),
        rootfs,
        network,
        volumes,
        auto_remove: false,
        detach: false,
        advanced: advanced_options(),
        ..Default::default()
    }
}

/// `BoxLite`'s host-side jailer cannot start a box on NixOS, for two distinct
/// reasons (both verified on real hardware; see `doc/sandbox.md` §5.2):
///
/// 1. **libcap**: without a system `bwrap` on `PATH`, `BoxLite` falls back to
///    its bundled `bwrap`, which can't find `libcap.so.2` (NixOS has no FHS lib
///    path). The production flake fixes this by putting nixpkgs `bubblewrap` on
///    `PATH`; a bare `cargo test` does not, so it still needs the env override.
/// 2. **CA dangling-symlink bind** (an upstream `BoxLite` bug): its
///    `system_ca_paths()` read-only-binds every host CA path that exists, and on
///    NixOS `/etc/ssl/certs/ca-certificates.crt` is a symlink into `/etc/static`
///    → `/nix/store`. `bwrap` binds the parent dir read-only first, exposing the
///    symlink as dangling (its target isn't bound in), then fails to create the
///    file's mount point — box start aborts. `SecurityOptions` exposes no
///    cert-path knob, so we cannot fix it from the integration layer without
///    disabling the host jailer entirely.
///
/// Because NixOS is a first-class deployment target (not just a dev host), we
/// **auto-disable the host jailer on NixOS** (detected via `/etc/NIXOS`) so the
/// backend works out of the box, and warn once that host-side hardening is off.
/// The microVM/KVM boundary — the primary isolation for untrusted guest code —
/// is unaffected; only the host-side shim hardening (seccomp/chroot/uid-drop) is
/// dropped. `OMINI_BOXLITE_INSECURE=1` forces the same on any other affected
/// host. Standard FHS deploys keep the full jailer. The real fix is upstream
/// (`system_ca_paths` should not bind a dir *and* a file inside it); see
/// `doc/sandbox.md` §5.2.
fn advanced_options() -> AdvancedBoxOptions {
    if jailer_unsupported_here() {
        AdvancedBoxOptions {
            security: SecurityOptions::disabled(),
            ..AdvancedBoxOptions::default()
        }
    } else {
        AdvancedBoxOptions::default()
    }
}

/// Whether the host-side jailer must be disabled on this machine: NixOS (where
/// the bundled-bwrap / CA-symlink bugs above make it fail) or an explicit
/// `OMINI_BOXLITE_INSECURE=1` override. Warns once, loudly, when it disables the
/// jailer — this is a real reduction in host-side defense-in-depth, so it must
/// never be silent (the KVM boundary still holds; see the fn doc above).
fn jailer_unsupported_here() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        let forced = std::env::var_os("OMINI_BOXLITE_INSECURE").is_some();
        // `/etc/NIXOS` is NixOS's own marker file for "this is a NixOS system".
        let nixos = std::path::Path::new("/etc/NIXOS").exists();
        if nixos || forced {
            let why = if forced {
                "OMINI_BOXLITE_INSECURE=1 is set"
            } else {
                "this is NixOS (upstream boxlite jailer is incompatible, doc/sandbox.md §5.2)"
            };
            tracing::warn!(
                "boxlite: host-side jailer DISABLED because {why}. The guest is \
                 still KVM/microVM-isolated; only host-side shim hardening \
                 (seccomp/chroot/uid-drop) is off."
            );
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::sandbox::{ResourceLimits, VolumeMount};
    use std::path::PathBuf;

    fn config() -> SandboxConfig {
        SandboxConfig::default()
    }

    #[test]
    fn exit_code_normalizes_signal_to_none() {
        assert_eq!(normalize_exit_code(0), Some(0));
        assert_eq!(normalize_exit_code(3), Some(3));
        assert_eq!(normalize_exit_code(-9), None, "signal death -> None");
    }

    #[test]
    fn empty_rootfs_defaults_to_image() {
        let opts = box_options(&config());
        assert!(matches!(opts.rootfs, RootfsSpec::Image(_)));
    }

    #[test]
    fn explicit_rootfs_is_passed_through() {
        let cfg = SandboxConfig {
            rootfs: "ubuntu:24.04".to_owned(),
            ..Default::default()
        };
        match box_options(&cfg).rootfs {
            RootfsSpec::Image(img) => assert_eq!(img, "ubuntu:24.04"),
            RootfsSpec::RootfsPath(p) => panic!("expected Image, got RootfsPath({p:?})"),
        }
    }

    #[test]
    fn network_policy_maps_to_spec() {
        let isolated = box_options(&SandboxConfig {
            network: NetworkPolicy::Isolated,
            ..Default::default()
        });
        assert!(matches!(isolated.network, NetworkSpec::Disabled));

        let open = box_options(&SandboxConfig {
            network: NetworkPolicy::Open,
            ..Default::default()
        });
        assert!(matches!(
            open.network,
            NetworkSpec::Enabled { allow_net } if allow_net.is_empty()
        ));

        let allow = box_options(&SandboxConfig {
            network: NetworkPolicy::AllowList(vec!["api.openai.com".to_owned()]),
            ..Default::default()
        });
        assert!(matches!(
            allow.network,
            NetworkSpec::Enabled { allow_net } if allow_net == ["api.openai.com"]
        ));
    }

    #[test]
    fn resources_and_volumes_map_through() {
        let cfg = SandboxConfig {
            resources: ResourceLimits {
                timeout: Duration::from_secs(30),
                memory_mb: Some(512),
                cpus: Some(2),
            },
            volumes: vec![VolumeMount {
                host_path: PathBuf::from("/host/data"),
                guest_path: PathBuf::from("/data"),
                read_only: false,
            }],
            ..Default::default()
        };
        let opts = box_options(&cfg);
        assert_eq!(opts.cpus, Some(2));
        assert_eq!(opts.memory_mib, Some(512));
        assert_eq!(opts.volumes.len(), 1);
        assert_eq!(opts.volumes[0].guest_path, "/data");
        assert!(!opts.auto_remove, "boxes must not auto-remove");
        assert!(!opts.detach);
    }

    #[test]
    fn workspace_becomes_a_rw_mount_and_cwd() {
        // §3.3: a bound workspace must land as cwd inside the guest, mirroring the
        // passthrough backend where cwd *is* the host workspace. Realized as a
        // read-write bind mount at a fixed guest path plus working_dir.
        let cfg = SandboxConfig {
            workspace: PathBuf::from("/home/user/repo"),
            ..Default::default()
        };
        let opts = box_options(&cfg);
        assert_eq!(opts.working_dir.as_deref(), Some(WORKSPACE_GUEST_PATH));
        let ws = opts
            .volumes
            .iter()
            .find(|v| v.guest_path == WORKSPACE_GUEST_PATH)
            .unwrap();
        assert_eq!(ws.host_path, "/home/user/repo");
        assert!(
            !ws.read_only,
            "workspace is writable (edits go to the repo)"
        );
    }

    #[test]
    fn empty_workspace_mounts_nothing_and_leaves_cwd() {
        // A default config carries no workspace; mounting host `/` or pinning cwd
        // would be wrong. Leave working_dir to the image and add no workspace
        // volume, so default-config boxes behave exactly as before this change.
        let opts = box_options(&SandboxConfig::default());
        assert!(
            opts.working_dir.is_none(),
            "no workspace -> image's own cwd"
        );
        assert!(
            opts.volumes
                .iter()
                .all(|v| v.guest_path != WORKSPACE_GUEST_PATH),
            "no workspace volume when workspace is unset"
        );
    }

    #[test]
    fn capabilities_reflect_boxlite() {
        // Compile-time: BoxLite must advertise the CoW filesystem-snapshot +
        // refcounted-GC properties it was chosen for (§4.1), and must NOT claim
        // live snapshot / hot fork it doesn't have.
        const {
            assert!(CAPABILITIES.filesystem_snapshot);
            assert!(CAPABILITIES.refcounted_gc);
            assert!(!CAPABILITIES.live_snapshot);
            assert!(!CAPABILITIES.hot_fork);
        }
    }

    // ── Integration tests: need a real KVM host + image pull (network), so they
    // are #[ignore]d and run manually on a supported host (doc/sandbox.md §7,
    // "manual testing"). Run with:
    //   cargo test --features sandbox-boxlite -- --ignored
    #[tokio::test]
    #[ignore = "needs KVM + image pull; run manually on a supported host"]
    async fn exec_echo_roundtrips() {
        let backend = BoxliteBackend::new().unwrap();
        let sb = backend.create(config()).await.unwrap();
        let out = sb
            .exec("echo hello", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(out.success());
        assert!(out.stdout.contains("hello"));
        sb.release().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs KVM + image pull; run manually on a supported host"]
    async fn snapshot_then_restore_is_isolated() {
        let backend = BoxliteBackend::new().unwrap();
        let parent = backend.create(config()).await.unwrap();
        parent
            .exec("echo seed > /marker", Duration::from_secs(30))
            .await
            .unwrap();

        let snap = parent.snapshot().await.unwrap();
        let child = backend.restore(&snap).await.unwrap();

        // Child inherits the parent's filesystem at snapshot time...
        let seen = child
            .exec("cat /marker", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(seen.stdout.contains("seed"));

        // ...but writes in the child do not leak back to the parent (CoW).
        child
            .exec("echo child > /marker", Duration::from_secs(30))
            .await
            .unwrap();
        let parent_view = parent
            .exec("cat /marker", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(parent_view.stdout.contains("seed"), "parent unchanged");

        child.release().await.unwrap();
        parent.release().await.unwrap();
    }

    // The session-scoped fork path (`doc/sandbox.md` §4.2): a parent registered
    // in the `SandboxManager` is forked through `fork_from`, yielding an isolated
    // CoW child. Verifies the manager wiring end-to-end on real hardware, not
    // just the backend primitive above.
    #[tokio::test]
    #[ignore = "needs KVM + image pull; run manually on a supported host"]
    async fn manager_fork_from_yields_isolated_child() {
        use crate::core::SessionId;
        use crate::sandbox::manager::SandboxManager;

        let mgr = SandboxManager::with_backend(Arc::new(BoxliteBackend::new().unwrap()));
        let parent_id = SessionId("parent".to_owned());
        let parent = mgr.backend().create(config()).await.unwrap();
        parent
            .exec("echo seed > /marker", Duration::from_secs(30))
            .await
            .unwrap();
        mgr.register(&parent_id, Arc::clone(&parent)).await;

        let (child, descriptor) = mgr.fork_from(&parent_id).await.unwrap();
        assert_eq!(descriptor.backend, "boxlite");

        // Child inherits the parent's filesystem at fork time...
        let seen = child
            .exec("cat /marker", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(seen.stdout.contains("seed"));

        // ...but its writes stay private (CoW): the parent is unchanged.
        child
            .exec("echo child > /marker", Duration::from_secs(30))
            .await
            .unwrap();
        let parent_view = parent
            .exec("cat /marker", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(parent_view.stdout.contains("seed"), "parent unchanged");

        child.release().await.unwrap();
        parent.release().await.unwrap();
    }

    // §3.3 on real hardware: a bound workspace is a live RW bind mount, and the
    // guest shell's cwd lands in it — so `pwd` is `/workspace`, a host file is
    // visible there, and a guest write shows up back on the host. This is the
    // one property (§9 Q6) that only a real guest can confirm; box_options unit
    // tests above only check the request shape, not that boxlite honours it.
    #[tokio::test]
    #[ignore = "needs KVM + image pull; run manually on a supported host"]
    async fn workspace_is_cwd_and_passes_through_to_host() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("from_host.txt"), "hello").unwrap();

        let cfg = SandboxConfig {
            workspace: ws.path().to_path_buf(),
            ..Default::default()
        };
        let sb = BoxliteBackend::new().unwrap().create(cfg).await.unwrap();

        // cwd is the workspace mount, and the host-authored file is visible there.
        let pwd = sb.exec("pwd", Duration::from_secs(30)).await.unwrap();
        assert_eq!(pwd.stdout.trim(), WORKSPACE_GUEST_PATH);
        let seen = sb
            .exec("cat from_host.txt", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(seen.stdout.contains("hello"), "host file visible in cwd");

        // A guest write passes through to the host workspace (RW bind mount, not
        // the CoW disk) — this is why fork isolation deliberately excludes it.
        sb.exec("echo guest > from_guest.txt", Duration::from_secs(30))
            .await
            .unwrap();
        let on_host = std::fs::read_to_string(ws.path().join("from_guest.txt")).unwrap();
        assert_eq!(
            on_host.trim(),
            "guest",
            "guest write reaches host workspace"
        );

        sb.release().await.unwrap();
    }
}
