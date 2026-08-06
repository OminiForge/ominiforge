//! Workspace development environments via direnv (`doc/architecture.md`).
//!
//! A workspace's `.envrc` is the single source of truth for its development
//! environment (nix flake, uv, … — the mechanism is irrelevant, direnv is the
//! common denominator). This module exports it through direnv and applies the
//! result as an environment overlay to everything a session spawns (shell
//! sandbox, MCP servers, language servers). The design hides direnv's
//! evaluation cost from the user:
//!
//! - **Preparation is background work** — recording a workspace
//!   (`SessionRegistry::record_workspace`) spawns a refresh, and every session
//!   assembly whose fast export stalls spawns one too. The expensive
//!   `use flake` evaluation is paid where nobody waits.
//! - **Sessions never block on a slow evaluation** — assembly tries
//!   `direnv export json` with a small budget; direnv's own cache (`.direnv/`,
//!   keyed on watched files) makes the warm path sub-second. On timeout or
//!   failure it falls back to the last exported snapshot
//!   ([`WorkspaceEnvCache`]) — stale beats empty — and a background refresh
//!   re-warms both caches for the next session.
//!
//! Freshness rides on direnv's own watch model (`.envrc` content plus files
//! like `flake.nix`/`flake.lock`): a changed input makes direnv re-evaluate,
//! so the fast path is always fresh when cheap, and the snapshot bridges
//! exactly the expensive-reevaluation window. Trust stays with direnv: a
//! `.envrc` must be `direnv allow`ed, we never bypass that.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How direnv is invoked and how long each path may wait for it. Defaults are
/// the production values; tests inject a mock command and tiny timeouts.
#[derive(Debug, Clone)]
pub(crate) struct EnvActivation {
    /// The direnv executable (or a test double's path).
    pub cmd: String,
    /// Budget for the assembly fast path. Warm direnv caches answer in well
    /// under a second, so 2s only ever trips on a cold or expensive
    /// re-evaluation — exactly the case the snapshot + background refresh
    /// exist for.
    pub fast_timeout: Duration,
    /// Budget for background preparation. A cold `use flake` evaluation can
    /// take minutes, and nobody is waiting on it.
    pub prepare_timeout: Duration,
}

impl Default for EnvActivation {
    fn default() -> Self {
        Self {
            cmd: "direnv".to_owned(),
            fast_timeout: Duration::from_secs(2),
            prepare_timeout: Duration::from_secs(300),
        }
    }
}

/// Why a `direnv export` failed. The distinctions drive whether a background
/// retry is worthwhile: a missing binary fails every retry, while a slow or
/// mid-edit `.envrc` may well succeed moments later.
#[derive(Debug)]
pub(crate) enum ExportError {
    /// The direnv process could not be started (not installed / not on PATH).
    Spawn(String),
    /// The export exceeded its budget (cold or expensive re-evaluation).
    Timeout,
    /// direnv ran but exited non-zero (blocked `.envrc`, evaluation error).
    Failed(String),
    /// The export's stdout was not the expected JSON shape.
    Parse(String),
    /// The export succeeded but the snapshot could not be persisted.
    Persist(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to run direnv: {e}"),
            Self::Timeout => write!(f, "direnv export timed out"),
            Self::Failed(stderr) => write!(f, "direnv failed: {stderr}"),
            Self::Parse(e) => write!(f, "failed to parse direnv output: {e}"),
            Self::Persist(e) => write!(f, "failed to persist env cache: {e}"),
        }
    }
}

/// Run `direnv export json` in `workspace`, bounded by `timeout`, and parse
/// the result into an environment overlay.
async fn direnv_export(
    workspace: &Path,
    cmd: &str,
    timeout: Duration,
) -> Result<BTreeMap<String, Option<String>>, ExportError> {
    let output = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(cmd)
            .arg("export")
            .arg("json")
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;
    let output = match output {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(ExportError::Spawn(e.to_string())),
        Err(_) => return Err(ExportError::Timeout),
    };
    if !output.status.success() {
        return Err(ExportError::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    parse_direnv_json(&output.stdout).map_err(|e| ExportError::Parse(e.to_string()))
}

/// Parse `direnv export json` into an overlay: a string value becomes
/// `Some(v)` (set), null becomes `None` (unset). direnv's own bookkeeping
/// (`DIRENV_*` — dump blobs, watch lists) and non-string values are dropped:
/// the overlay is applied to every subprocess the session spawns, and the
/// bookkeeping belongs to direnv's state, not to a child's environment.
pub(crate) fn parse_direnv_json(bytes: &[u8]) -> anyhow::Result<BTreeMap<String, Option<String>>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(BTreeMap::new());
    }
    let env: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(bytes)?;
    Ok(env
        .into_iter()
        .filter(|(key, _)| !key.starts_with("DIRENV_"))
        .filter_map(|(key, value)| {
            if value.is_null() {
                Some((key, None))
            } else {
                value.as_str().map(|value| (key, Some(value.to_owned())))
            }
        })
        .collect())
}

/// FNV-1a 64-bit hash of `bytes`, lower-hex encoded to 16 chars. A fixed,
/// version-stable algorithm (offset basis + prime are constants), so ids and
/// cache filenames derived from it stay the same across builds and toolchains.
/// Shared with `gateway::workspace::WorkspaceId`, whose persisted values must
/// never shift — do not change this.
#[must_use]
pub(crate) fn fnv1a_hex(bytes: &[u8]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// A per-workspace snapshot of the last successful direnv export, persisted as
/// `<root>/workspaces-env/<workspace-id>.json` (the id hashes the canonical
/// workspace path — the same scheme as `gateway::workspace::WorkspaceId`).
///
/// The snapshot bridges the window where direnv itself is expensive (cold
/// `.direnv/` cache, or watched files just changed): sessions fall back to it
/// instead of starting with no environment at all. Reads are tolerant — a
/// missing, corrupt, or foreign-workspace file is simply a miss.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceEnvCache {
    dir: PathBuf,
}

/// The persisted snapshot shape.
#[derive(Debug, Serialize, Deserialize)]
struct CachedEnv {
    /// The canonical workspace the export was taken in (mismatch = miss).
    workspace: PathBuf,
    /// Unix seconds when the export completed.
    prepared_at: u64,
    /// The environment overlay.
    env: BTreeMap<String, Option<String>>,
}

impl WorkspaceEnvCache {
    /// Root the cache at `<root>/workspaces-env` — the highest-priority config
    /// root, so the CLI and the gateway agree on one location for a launch.
    pub(crate) fn anchored_at(root: &Path) -> Self {
        Self {
            dir: root.join("workspaces-env"),
        }
    }

    fn path_for(&self, workspace: &Path) -> PathBuf {
        self.dir.join(format!(
            "{}.json",
            fnv1a_hex(workspace.as_os_str().as_encoded_bytes())
        ))
    }

    /// The cached snapshot for `workspace`, or `None` on any miss.
    fn load(&self, workspace: &Path) -> Option<CachedEnv> {
        let text = std::fs::read_to_string(self.path_for(workspace)).ok()?;
        let cached: CachedEnv = serde_json::from_str(&text).ok()?;
        (cached.workspace == workspace).then_some(cached)
    }

    /// Persist `env` as `workspace`'s snapshot. Atomic (temp file + rename),
    /// so a concurrent reader never observes a partial write.
    fn store(
        &self,
        workspace: &Path,
        env: &BTreeMap<String, Option<String>>,
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let cached = CachedEnv {
            workspace: workspace.to_path_buf(),
            prepared_at: now_unix(),
            env: env.clone(),
        };
        let target = self.path_for(workspace);
        let tmp = target.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&cached)?)?;
        std::fs::rename(tmp, target)
    }

    /// Drop `workspace`'s snapshot, if any (workspace-config GC).
    pub(crate) fn remove(&self, workspace: &Path) {
        let _ = std::fs::remove_file(self.path_for(workspace));
    }

    /// The cached env overlay for `workspace`, or `None` on any miss. This is
    /// the read-side handle for consumers that need the workspace's exported
    /// environment WITHOUT triggering a direnv evaluation (e.g. the workspace
    /// config UI's install probe, which must not block on a `use flake`
    /// evaluation). Stale-beats-empty applies here too: the snapshot is what
    /// the last session actually ran with.
    pub(crate) fn snapshot(
        &self,
        workspace: &Path,
    ) -> Option<std::collections::BTreeMap<String, Option<String>>> {
        self.load(workspace).map(|c| c.env)
    }
}

/// The environment overlay a session in `workspace` should run with.
///
/// Fast path: export through direnv with a small budget — fresh and sub-second
/// whenever direnv's own cache is warm (the common case). Slow path: fall back
/// to the last snapshot and spawn a background refresh so the NEXT session is
/// warm. Either way assembly never blocks on an expensive evaluation
/// (`doc/architecture.md`). A missing/broken direnv or a blocked `.envrc` degrades to
/// "no workspace env" with an actionable warning, never a failed session.
pub(crate) async fn session_env(
    workspace: &Path,
    cache: Option<&WorkspaceEnvCache>,
    activation: &EnvActivation,
) -> BTreeMap<String, Option<String>> {
    let envrc = workspace.join(".envrc");
    if !envrc.is_file() {
        return BTreeMap::new();
    }

    let started = std::time::Instant::now();
    match direnv_export(workspace, &activation.cmd, activation.fast_timeout).await {
        Ok(env) => {
            // Routine success: not worth an operator's attention by default.
            tracing::debug!(
                vars = env.len(),
                envrc = %envrc.display(),
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "loaded env via direnv"
            );
            if let Some(cache) = cache
                && let Err(e) = cache.store(workspace, &env)
            {
                tracing::warn!("failed to persist env cache: {e}");
            }
            env
        }
        Err(e) => {
            // A background retry is worthwhile unless the tool itself is
            // missing — a slow or mid-edit `.envrc` may succeed moments later.
            if !matches!(e, ExportError::Spawn(_))
                && let Some(cache) = cache
            {
                spawn_refresh(workspace.to_path_buf(), cache.clone(), activation.clone());
            }
            if let Some(cached) = cache.and_then(|c| c.load(workspace)) {
                tracing::warn!(
                    "direnv export for {} unavailable ({e}, after {}ms); \
                     using the env prepared {}s ago",
                    envrc.display(),
                    started.elapsed().as_millis(),
                    now_unix().saturating_sub(cached.prepared_at)
                );
                cached.env
            } else {
                tracing::warn!(
                    "direnv export for {} unavailable ({e}); running without the workspace \
                     env — check `direnv allow` and that the .envrc evaluates in a shell",
                    envrc.display()
                );
                BTreeMap::new()
            }
        }
    }
}

/// Workspaces with a refresh currently running (process-local), so concurrent
/// assemblies / workspace records don't pile duplicate direnv evaluations onto
/// the same project.
static IN_FLIGHT: std::sync::LazyLock<Mutex<HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Lock the in-flight set, recovering from poisoning: a panicked refresh task
/// must not deadlock every future refresh.
fn in_flight() -> std::sync::MutexGuard<'static, HashSet<PathBuf>> {
    IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Spawn a background cache refresh for `workspace` unless one is already
/// running. A detached task has no session to warn, so the outcome is logged:
/// failures loud, success one line.
pub(crate) fn spawn_refresh(
    workspace: PathBuf,
    cache: WorkspaceEnvCache,
    activation: EnvActivation,
) {
    if !in_flight().insert(workspace.clone()) {
        return;
    }
    tokio::spawn(async move {
        let outcome = refresh_cache(&workspace, &cache, &activation).await;
        in_flight().remove(&workspace);
        match outcome {
            Ok(()) => {
                tracing::debug!(workspace = %workspace.display(), "prepared workspace env");
            }
            Err(e) => {
                tracing::warn!(workspace = %workspace.display(), "background env refresh failed: {e}");
            }
        }
    });
}

/// Export `workspace`'s environment with the generous prepare budget and
/// persist it. The background task body; `pub(crate)` so tests can drive it
/// directly (fire-and-forget spawns are awkward to assert on).
pub(crate) async fn refresh_cache(
    workspace: &Path,
    cache: &WorkspaceEnvCache,
    activation: &EnvActivation,
) -> Result<(), ExportError> {
    let env = direnv_export(workspace, &activation.cmd, activation.prepare_timeout).await?;
    cache
        .store(workspace, &env)
        .map_err(|e| ExportError::Persist(e.to_string()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A mock direnv that answers instantly with a payload covering a set var,
    /// an unset var, and direnv bookkeeping (which must be filtered out).
    const MOCK_GOOD: &str = r#"#!/bin/sh
printf '%s' '{"MOCK_VAR":"on","MOCK_COMPLEX":"quote: \" line:\n","MOCK_UNSET":null,"DIRENV_DIFF":"bookkeeping","IGNORED":false}'
"#;

    /// A mock direnv that fails the way a blocked/broken `.envrc` does.
    const MOCK_FAIL: &str = r#"#!/bin/sh
echo "direnv: .envrc is blocked" >&2
exit 1
"#;

    /// Write an executable mock direnv script and return its path. Scripts are
    /// content-addressed and written once per test process, then only ever
    /// executed: execve of a freshly-written script races concurrent forks on
    /// Linux (ETXTBSY — it flaked these very tests), so no mock file is ever
    /// rewritten mid-run. The per-process dir is kept (not deleted) so the
    /// detached background-refresh tasks earlier tests spawned never lose
    /// their script.
    fn mock_direnv(_dir: &Path, body: &str) -> PathBuf {
        static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        static LOCK: Mutex<()> = Mutex::new(());
        let dir = DIR.get_or_init(|| {
            let tmp = std::env::temp_dir().join(format!("omini-env-mocks-{}", std::process::id()));
            std::fs::create_dir_all(&tmp).unwrap();
            tmp
        });
        let path = dir.join(fnv1a_hex(body.as_bytes()));
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !path.exists() {
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn activation(cmd: &Path, fast: Duration, prepare: Duration) -> EnvActivation {
        EnvActivation {
            cmd: cmd.to_string_lossy().into_owned(),
            fast_timeout: fast,
            prepare_timeout: prepare,
        }
    }

    /// A canonical workspace with an `.envrc`, plus its parent tempdir.
    fn envrc_workspace(tmp: &tempfile::TempDir) -> PathBuf {
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".envrc"), "export MOCK_VAR=on\n").unwrap();
        ws.canonicalize().unwrap()
    }

    /// direnv's JSON export becomes a per-workspace overlay; non-string values
    /// are ignored, and `DIRENV_*` bookkeeping never reaches a child env.
    #[test]
    fn direnv_json_parses_string_env_overlay() {
        assert!(parse_direnv_json(b"").unwrap().is_empty());

        let json = br#"{"OMINI_SIMPLE":"a","OMINI_COMPLEX":"quote: \" slash: \\ line:\n","OMINI_UNSET":null,"DIRENV_DIFF":"blob","DIRENV_WATCHES":"blob","IGNORED":false}"#;

        let env = parse_direnv_json(json).unwrap();

        assert_eq!(
            env.get("OMINI_SIMPLE").and_then(|value| value.as_deref()),
            Some("a")
        );
        assert_eq!(
            env.get("OMINI_COMPLEX").and_then(|value| value.as_deref()),
            Some("quote: \" slash: \\ line:\n")
        );
        assert_eq!(env.get("OMINI_UNSET"), Some(&None));
        assert!(!env.contains_key("IGNORED"));
        assert!(!env.keys().any(|k| k.starts_with("DIRENV_")));
    }

    /// The fast path: a quick mock export lands in the overlay AND is
    /// persisted as the workspace's snapshot.
    #[tokio::test]
    async fn fast_path_exports_filters_and_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = envrc_workspace(&tmp);
        let mock = mock_direnv(tmp.path(), MOCK_GOOD);
        let cache = WorkspaceEnvCache::anchored_at(tmp.path());

        let env = session_env(
            &ws,
            Some(&cache),
            &activation(&mock, Duration::from_secs(5), Duration::from_secs(5)),
        )
        .await;

        assert_eq!(env.get("MOCK_VAR").and_then(|v| v.as_deref()), Some("on"));
        assert_eq!(env.get("MOCK_UNSET"), Some(&None));
        assert!(!env.contains_key("DIRENV_DIFF"));
        let cached = cache.load(&ws).expect("snapshot persisted");
        assert_eq!(cached.env, env);
    }

    /// The slow path: an export that exceeds the fast budget returns the LAST
    /// snapshot instead of making the session wait — stale beats empty.
    #[tokio::test]
    async fn slow_export_falls_back_to_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = envrc_workspace(&tmp);
        // Sleeps past the fast budget, then answers (the background refresh
        // this test triggers rewrites the snapshot with the fresh export —
        // fine; the returned overlay is the snapshot taken at load time).
        let mock = mock_direnv(
            tmp.path(),
            "#!/bin/sh\nsleep 0.3\nprintf '%s' '{\"MOCK_VAR\":\"fresh\"}'\n",
        );
        let cache = WorkspaceEnvCache::anchored_at(tmp.path());
        let mut seeded = BTreeMap::new();
        seeded.insert("SEEDED".to_owned(), Some("1".to_owned()));
        cache.store(&ws, &seeded).unwrap();

        let env = session_env(
            &ws,
            Some(&cache),
            &activation(&mock, Duration::from_millis(50), Duration::from_secs(5)),
        )
        .await;

        // The degraded path must return the snapshot, not fail the session.
        assert_eq!(env, seeded);
    }

    /// `refresh_cache` is the background task body: export with the prepare
    /// budget and persist — driven directly here.
    #[tokio::test]
    async fn refresh_cache_persists_export() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = envrc_workspace(&tmp);
        let mock = mock_direnv(tmp.path(), MOCK_GOOD);
        let cache = WorkspaceEnvCache::anchored_at(tmp.path());

        refresh_cache(
            &ws,
            &cache,
            &activation(&mock, Duration::from_secs(5), Duration::from_secs(5)),
        )
        .await
        .unwrap();

        let cached = cache.load(&ws).expect("snapshot persisted");
        assert_eq!(
            cached.env.get("MOCK_VAR").and_then(|v| v.as_deref()),
            Some("on")
        );
    }

    /// A failing export: with a snapshot the session uses it; without one it
    /// runs with no workspace env — either way with an actionable warning,
    /// never a failed session.
    #[tokio::test]
    async fn failed_export_uses_snapshot_or_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = envrc_workspace(&tmp);
        let mock = mock_direnv(tmp.path(), MOCK_FAIL);
        let cache = WorkspaceEnvCache::anchored_at(tmp.path());
        let act = activation(&mock, Duration::from_secs(5), Duration::from_millis(50));

        // Without a snapshot: empty overlay, never a failed session.
        let env = session_env(&ws, Some(&cache), &act).await;
        assert!(env.is_empty());

        // With a snapshot: the snapshot wins.
        let mut seeded = BTreeMap::new();
        seeded.insert("SEEDED".to_owned(), Some("1".to_owned()));
        cache.store(&ws, &seeded).unwrap();
        let env = session_env(&ws, Some(&cache), &act).await;
        assert_eq!(env, seeded);
    }

    /// No `.envrc` means direnv is never even spawned — the zero-cost path.
    #[tokio::test]
    async fn missing_envrc_never_invokes_direnv() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();
        let marker = tmp.path().join("invoked");
        let mock = mock_direnv(
            tmp.path(),
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let env = session_env(
            &ws,
            Some(&WorkspaceEnvCache::anchored_at(tmp.path())),
            &activation(&mock, Duration::from_secs(5), Duration::from_secs(5)),
        )
        .await;

        assert!(env.is_empty());
        assert!(
            !marker.exists(),
            "direnv must not be spawned without an .envrc"
        );
    }

    /// Real-direnv smoke test: a real `.envrc` (allowed via a throwaway
    /// `DIRENV_CONFIG`) lands in the overlay. Skipped unless `OMINI_SMOKE_WS`
    /// points at a prepared directory — run it with:
    ///
    /// ```sh
    /// export DC=$(mktemp -d) WS=$(mktemp -d)
    /// echo 'export OMINI_SMOKE=hello' > "$WS/.envrc"
    /// DIRENV_CONFIG=$DC direnv allow "$WS/.envrc"
    /// DIRENV_CONFIG=$DC OMINI_SMOKE_WS=$WS cargo test env::tests::real_direnv -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "needs real direnv + a pre-allowed .envrc (see comment)"]
    async fn real_direnv_export_roundtrip() {
        let Ok(ws) = std::env::var("OMINI_SMOKE_WS") else {
            return;
        };
        let ws = PathBuf::from(ws).canonicalize().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cache = WorkspaceEnvCache::anchored_at(tmp.path());
        let env = session_env(&ws, Some(&cache), &EnvActivation::default()).await;
        assert_eq!(
            env.get("OMINI_SMOKE").and_then(|v| v.as_deref()),
            Some("hello")
        );
        assert!(cache.load(&ws).is_some());
    }
}
