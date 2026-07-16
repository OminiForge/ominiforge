//! [`SandboxManager`]: the per-session owner of sandbox instances
//! (`doc/sandbox.md` §3.2).
//!
//! A session's sandbox is a first-class, session-scoped resource — it outlives
//! the thread (actor) that drives the session and is only reclaimed when the
//! session is deleted. The manager holds one backend for the whole process
//! (chosen once from deployment config) and a map from session id to its live
//! sandbox handle, so the fork path can reach a parent's sandbox by id without
//! routing through its actor.
//!
//! Backend selection is a deployment property (does this host have KVM?), not a
//! per-task one, so it lives here rather than in a profile.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::passthrough::PassthroughBackend;
use super::{Sandbox, SandboxBackend, SandboxError, fork_sandbox};
use crate::core::SessionId;
use crate::session::SandboxDescriptor;

/// Which sandbox backend a deployment wants (`doc/sandbox.md` §3.2, §8).
///
/// A host-level, OS-agnostic choice: the same config value means the same thing
/// on every platform; whether boxlite can actually start is a property of the
/// host (KVM, jailer deps), handled at construction — not baked into the choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendChoice {
    /// Host execution, zero isolation. Safe and universal — the default so no
    /// deployment silently believes it has isolation it does not.
    #[default]
    Passthrough,
    /// Require the boxlite microVM backend. If it cannot start (no KVM, missing
    /// jailer deps, feature not compiled), construction fails **loud** — an
    /// explicit isolation request must never silently degrade.
    Boxlite,
    /// Prefer boxlite, fall back to passthrough with a loud warning when it
    /// cannot start. Opt-in "best-effort isolation" for heterogeneous fleets.
    Auto,
}

/// Owns every session's sandbox for the process lifetime.
pub struct SandboxManager {
    backend: Arc<dyn SandboxBackend>,
    live: Mutex<HashMap<SessionId, Arc<dyn Sandbox>>>,
}

impl SandboxManager {
    /// Build a manager for a deployment's [`SandboxBackendChoice`], reporting
    /// non-fatal diagnostics (e.g. an `Auto` fallback) through `on_warn`
    /// (`doc/sandbox.md` §3.2). Backend selection is OS-agnostic here; host
    /// specifics (KVM, jailer deps, image pulls) surface as a boxlite start
    /// error, which each variant handles per its contract.
    ///
    /// # Errors
    /// [`SandboxError`] when [`SandboxBackendChoice::Boxlite`] is requested but
    /// boxlite cannot start (or was not compiled in).
    pub fn from_choice(
        choice: SandboxBackendChoice,
        on_warn: &(dyn Fn(&str) + Sync),
    ) -> Result<Self, SandboxError> {
        match choice {
            SandboxBackendChoice::Passthrough => Ok(Self::passthrough()),
            SandboxBackendChoice::Boxlite => Ok(Self::with_backend(Self::boxlite_backend()?)),
            SandboxBackendChoice::Auto => match Self::boxlite_backend() {
                Ok(backend) => Ok(Self::with_backend(backend)),
                Err(e) => {
                    on_warn(&format!(
                        "sandbox: boxlite unavailable ({e}); falling back to passthrough \
                         (no isolation)"
                    ));
                    Ok(Self::passthrough())
                }
            },
        }
    }

    /// Construct the boxlite backend, or explain why it is unavailable. Behind a
    /// `cfg` so a build without the `sandbox-boxlite` feature still compiles and
    /// gives a clear "not compiled in" error rather than a link failure.
    #[cfg(feature = "sandbox-boxlite")]
    fn boxlite_backend() -> Result<Arc<dyn SandboxBackend>, SandboxError> {
        Ok(Arc::new(super::boxlite::BoxliteBackend::new()?))
    }

    #[cfg(not(feature = "sandbox-boxlite"))]
    #[allow(clippy::unnecessary_wraps)]
    fn boxlite_backend() -> Result<Arc<dyn SandboxBackend>, SandboxError> {
        Err(SandboxError::Unsupported(
            "boxlite backend not compiled in (rebuild with --features sandbox-boxlite)",
        ))
    }

    /// Build a manager over the passthrough backend (zero isolation, host
    /// execution). This is the default until a snapshot-capable backend
    /// (boxlite) is selectable via config.
    #[must_use]
    pub fn passthrough() -> Self {
        Self::with_backend(Arc::new(PassthroughBackend::new()))
    }

    /// Build a manager over an explicit backend.
    #[must_use]
    pub fn with_backend(backend: Arc<dyn SandboxBackend>) -> Self {
        Self {
            backend,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// A clone of the backend handle, for the assembly layer to build a
    /// session's sandbox from (`app::assemble`). The manager and the assembled
    /// sandbox thus share one backend, so a later `fork` restores onto the same
    /// `CoW` chain.
    #[must_use]
    pub fn backend(&self) -> Arc<dyn SandboxBackend> {
        Arc::clone(&self.backend)
    }

    /// Register `session`'s live sandbox (the one `assemble` built from this
    /// manager's backend), so later lookups — notably `fork` — can reach it by
    /// id. Replaces any existing entry (a resumed session re-registers).
    pub async fn register(&self, session: &SessionId, sandbox: Arc<dyn Sandbox>) {
        self.live.lock().await.insert(session.clone(), sandbox);
    }

    /// The live sandbox handle for `session`, if one is registered.
    pub async fn get(&self, session: &SessionId) -> Option<Arc<dyn Sandbox>> {
        self.live.lock().await.get(session).cloned()
    }

    /// Fork `parent`'s sandbox into a fresh, independently-writable child and
    /// return the handle plus the descriptor to persist (`doc/sandbox.md` §4.2).
    /// Does *not* register the child — the caller registers it once the child
    /// session id is minted (`register`).
    ///
    /// Requires `parent` to have a live sandbox and the backend to support
    /// filesystem snapshots. On a backend that cannot snapshot (passthrough),
    /// [`fork_sandbox`] returns [`SandboxError::Unsupported`] and the caller
    /// falls back to giving the child its own fresh sandbox on the inherited
    /// workspace.
    ///
    /// # Errors
    /// [`SandboxError::Unsupported`] if the parent is unknown or the backend
    /// cannot snapshot; otherwise a snapshot/restore fault.
    pub async fn fork_from(
        &self,
        parent: &SessionId,
    ) -> Result<(Arc<dyn Sandbox>, SandboxDescriptor), SandboxError> {
        let parent_sandbox = self.get(parent).await.ok_or(SandboxError::Unsupported(
            "cannot fork: parent session has no live sandbox",
        ))?;
        let sandbox = fork_sandbox(self.backend.as_ref(), &parent_sandbox).await?;
        let descriptor = SandboxDescriptor {
            backend: self.backend.name().to_owned(),
            id: None,
        };
        Ok((sandbox, descriptor))
    }

    /// Release `session`'s sandbox and drop its handle (`doc/sandbox.md` §3.2).
    ///
    /// Bound to session *deletion*, not thread eviction. There is no deletion
    /// path yet, so nothing calls this — its trigger and the surrounding GC land
    /// with Step 5 (`doc/sandbox.md` §8). A no-op if `session` has no registered
    /// sandbox.
    ///
    /// # Errors
    /// Propagates a backend `release` failure.
    pub async fn release(&self, session: &SessionId) -> Result<(), SandboxError> {
        let sandbox = self.live.lock().await.remove(session);
        if let Some(sandbox) = sandbox {
            sandbox.release().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::sandbox::SandboxConfig;
    use std::path::PathBuf;

    async fn passthrough_sandbox(workspace: PathBuf) -> Arc<dyn Sandbox> {
        PassthroughBackend::new()
            .create(SandboxConfig {
                workspace,
                ..SandboxConfig::default()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn register_then_get_returns_same_handle() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SandboxManager::passthrough();
        let sid = SessionId("s1".to_owned());
        let sandbox = passthrough_sandbox(dir.path().to_path_buf()).await;

        mgr.register(&sid, Arc::clone(&sandbox)).await;
        let fetched = mgr.get(&sid).await.unwrap();
        assert!(Arc::ptr_eq(&sandbox, &fetched));
    }

    #[tokio::test]
    async fn get_unknown_session_is_none() {
        let mgr = SandboxManager::passthrough();
        assert!(mgr.get(&SessionId("nope".to_owned())).await.is_none());
    }

    #[tokio::test]
    async fn fork_on_passthrough_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SandboxManager::passthrough();
        let parent = SessionId("parent".to_owned());
        mgr.register(&parent, passthrough_sandbox(dir.path().to_path_buf()).await)
            .await;

        // Passthrough cannot snapshot, so fork must fail loud (the caller then
        // falls back to a fresh child sandbox — verified at the registry layer).
        let result = mgr.fork_from(&parent).await;
        assert!(matches!(result, Err(SandboxError::Unsupported(_))));
    }

    #[tokio::test]
    async fn fork_unknown_parent_is_unsupported() {
        let mgr = SandboxManager::passthrough();
        let result = mgr.fork_from(&SessionId("ghost".to_owned())).await;
        assert!(matches!(result, Err(SandboxError::Unsupported(_))));
    }

    #[tokio::test]
    async fn release_drops_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SandboxManager::passthrough();
        let sid = SessionId("s1".to_owned());
        mgr.register(&sid, passthrough_sandbox(dir.path().to_path_buf()).await)
            .await;

        mgr.release(&sid).await.unwrap();
        assert!(mgr.get(&sid).await.is_none());
    }

    #[test]
    fn choice_passthrough_builds_passthrough() {
        let mgr = SandboxManager::from_choice(SandboxBackendChoice::Passthrough, &|_| {}).unwrap();
        assert_eq!(mgr.backend().name(), "passthrough");
    }

    #[cfg(not(feature = "sandbox-boxlite"))]
    #[test]
    fn choice_boxlite_without_feature_fails_loud() {
        // An explicit isolation request on a build that cannot provide it must
        // error, never silently degrade.
        let result = SandboxManager::from_choice(SandboxBackendChoice::Boxlite, &|_| {});
        assert!(matches!(result, Err(SandboxError::Unsupported(_))));
    }

    #[cfg(not(feature = "sandbox-boxlite"))]
    #[test]
    fn choice_auto_without_feature_warns_and_falls_back() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let warned = AtomicBool::new(false);
        let mgr = SandboxManager::from_choice(SandboxBackendChoice::Auto, &|_| {
            warned.store(true, Ordering::SeqCst);
        })
        .unwrap();
        assert_eq!(mgr.backend().name(), "passthrough");
        assert!(warned.load(Ordering::SeqCst), "auto fallback must warn");
    }

    #[test]
    fn choice_deserializes_from_snake_case() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            sandbox_backend: SandboxBackendChoice,
        }
        let parsed: Wrap = toml::from_str("sandbox_backend = \"boxlite\"").unwrap();
        assert_eq!(parsed.sandbox_backend, SandboxBackendChoice::Boxlite);
        assert_eq!(
            SandboxBackendChoice::default(),
            SandboxBackendChoice::Passthrough
        );
    }

    // Real-hardware: `sandbox_backend = "boxlite"` config builds a working
    // boxlite manager that can exec. Needs KVM + image pull, so #[ignore]d
    // (`doc/sandbox.md` §8). Run: cargo test --features sandbox-boxlite -- --ignored
    #[cfg(feature = "sandbox-boxlite")]
    #[tokio::test]
    #[ignore = "needs KVM + image pull; run manually on a supported host"]
    async fn choice_boxlite_builds_working_manager() {
        let mgr = SandboxManager::from_choice(SandboxBackendChoice::Boxlite, &|_| {}).unwrap();
        assert_eq!(mgr.backend().name(), "boxlite");
        let sid = SessionId("s1".to_owned());
        let sandbox = mgr
            .backend()
            .create(SandboxConfig::default())
            .await
            .unwrap();
        mgr.register(&sid, Arc::clone(&sandbox)).await;
        let out = sandbox
            .exec("echo ok", std::time::Duration::from_secs(30))
            .await
            .unwrap();
        assert!(out.stdout.contains("ok"));
    }
}
