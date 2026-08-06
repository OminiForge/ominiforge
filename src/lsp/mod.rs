//! LSP client: language-server lifecycle, JSON-RPC over `Content-Length`-framed
//! stdio, and the diagnostics assist that rides on `read`/`edit`/`write`
//! (`doc/lsp.md`).
//!
//! Unlike [`crate::mcp`], LSP is **not** a model-facing tool this phase. The
//! [`LspManager`] is a background helper: after a file op succeeds, the tool
//! hands the manager the touched path and its current text; the manager syncs
//! the doc to the right language server and returns the diagnostics the server
//! publishes, which the tool appends to its output. See [`client`] for the wire
//! machinery.
//!
//! ## Performance (the design's whole point)
//!
//! Language servers index slowly, so the manager is built to never stall a file
//! op on indexing:
//! - **Unsupported fast path** — a path whose extension no server claims returns
//!   `None` immediately, no spawn, no wait.
//! - **Lazy start, bounded init** — the first touch of a language spawns its
//!   server; the handshake is bounded by `init_timeout_ms`, and full indexing
//!   continues in the background regardless.
//! - **Warm reuse** — the server (and its open docs) live for the whole
//!   session, so subsequent edits are a `didChange` + a sub-second republish.
//! - **Bounded diagnostics wait** — each op waits at most `diag_timeout_ms`.

mod client;
pub(crate) mod config;
mod protocol;
pub(crate) mod registry;
mod service;

pub use service::{
    DEFAULT_DOC_IDLE_CLOSE, DEFAULT_RECLAIM_GRACE, LspRouter, LspService, ProcessLspService,
};

pub use client::{LspClient, LspError, uri_for};
pub use config::{ConfigError, LspConfig, LspServerConfig};
pub use protocol::{Diagnostic, DiagnosticSeverity};

/// A [`Diagnostic`] tagged with the name of the server that produced it, so a
/// multi-server language's aggregated block can attribute each issue
/// (`doc/lsp.md` §5).
#[derive(Debug, Clone)]
pub struct SourcedDiagnostic {
    /// The server's config `name`.
    pub server: String,
    /// The diagnostic as published.
    pub diagnostic: Diagnostic,
}

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// One server's runtime state, surfaced to the UI (`RuntimeInfo.lsp`,
/// `doc/lsp.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerState {
    /// Spawned and past the handshake, but not yet known ready (no successful
    /// diagnostics yet — heuristic (a1), `doc/lsp.md` §5.2). The transient the
    /// input-area "indexing…" indicator reads.
    Starting,
    /// Live client; answering diagnostics.
    Running,
    /// Last start/sync failed; in the retry cooldown (`doc/lsp.md` §4.6).
    Failed,
}

/// A snapshot of one server for display.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServerStatus {
    /// Config `name`.
    pub name: String,
    /// Extensions it handles.
    pub extensions: Vec<String>,
    /// Current state.
    pub state: ServerState,
}

/// Owns every language server for a session.
///
/// Routes a file op's diagnostics request to the right one. Held alive for the
/// session's lifetime (like `mcp_clients`); dropping it kills the server
/// subprocesses.
pub struct LspManager {
    /// The process-level owner of shared server instances (`doc/lsp.md`
    /// §5.2): every session under the same root routes to the same clients.
    /// Behind the [`LspRouter`] trait so the concrete implementation can be
    /// swapped (e.g. a test double) without changing this struct.
    router: Arc<dyn LspRouter>,
    /// This session's root — the `root_uri` the shared servers index.
    workspace: PathBuf,
    env_overlay: BTreeMap<String, Option<String>>,
    /// The enabled server configs this session routes among (the merged
    /// registry + layered `lsp.toml`). Instances are shared via `service`.
    configs: Vec<LspServerConfig>,
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspManager")
            .field("workspace", &self.workspace)
            .field("configs", &self.configs.len())
            .finish_non_exhaustive()
    }
}

impl LspManager {
    /// Build a per-session view over the shared `service`, rooted at
    /// `workspace`. Spawns nothing yet — servers start lazily on first touch.
    /// Returns `None` when no servers are configured, so callers skip the
    /// assist entirely with zero overhead.
    ///
    /// Configured servers are logged at setup (mirrors
    /// [`crate::mcp::connect_all`]) so a present-but-bad `lsp.toml` is at least
    /// visible; per-start failures surface later via `tracing` (§12 fail-loud).
    #[must_use]
    pub fn new(
        router: Arc<dyn LspRouter>,
        config: &LspConfig,
        workspace: PathBuf,
        env_overlay: BTreeMap<String, Option<String>>,
    ) -> Option<Arc<Self>> {
        if config.servers.is_empty() {
            return None;
        }
        for server in &config.servers {
            tracing::info!(
                server = %server.name,
                extensions = %server.extensions.join(", "),
                "lsp: configured (starts on first touch)"
            );
        }
        Some(Arc::new(Self {
            router,
            workspace,
            env_overlay,
            configs: config.servers.clone(),
        }))
    }

    /// The configs of every enabled server that handles `path`'s extension
    /// (config order). A language may have several servers (`pyright` +
    /// `ruff`); all of them receive the doc and contribute diagnostics.
    fn servers_for(&self, path: &Path) -> Vec<&LspServerConfig> {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return Vec::new();
        };
        self.configs
            .iter()
            .filter(|c| c.extensions.iter().any(|e| e == ext))
            .collect()
    }

    /// A display snapshot of the servers this session's root has **activated**
    /// (`doc/lsp.md` §5.1). Shared: any session under the same root sees the
    /// same server list, because the servers themselves are shared
    /// (`doc/lsp.md` §5.2). Cheap to call; the UI reads it alongside the rest
    /// of `RuntimeInfo`, which refreshes on turn settle — exactly when a file
    /// op may have changed a server's state.
    pub async fn status(&self) -> Vec<ServerStatus> {
        self.router.status(&self.workspace).await
    }

    /// Get diagnostics for `abs_path` given its current `text`, or `None` when
    /// no server handles it. `abs_path` must be absolute (a resolved workspace
    /// path); `text` is the file's content the op just read or wrote.
    ///
    /// Never returns an error: a spawn/handshake/timeout failure yields
    /// `Some(vec![])`-or-`None` semantics folded into "no diagnostics to
    /// attach" (`None`), because the assist is best-effort and must not turn a
    /// successful file op into a failure. `Some(diags)` — possibly empty, which
    /// means "server checked it, clean" — is attached; `None` means "no server
    /// / couldn't get an answer", and the tool attaches nothing. A mid-session
    /// server death additionally drops the cached client (reported on stderr),
    /// so a later op respawns it through the start-failure cooldown.
    pub async fn diagnostics(&self, abs_path: &Path, text: &str) -> Option<Vec<SourcedDiagnostic>> {
        let configs = self.servers_for(abs_path);
        if configs.is_empty() {
            return None;
        }
        let uri = uri_for(abs_path);
        let language_id = abs_path
            .extension()
            .and_then(|e| e.to_str())
            .map_or("", language_id_for);

        // Query every matching server concurrently; each contributes its
        // diagnostics, tagged with the server's name so the render can
        // attribute them (`via pyright` / `via ruff`). A server that fails to
        // start or sync forfeits only its own diagnostics, never the others'.
        // `join_all` drives the borrows concurrently without needing 'static.
        let results = futures_util::future::join_all(configs.iter().map(|config| {
            let uri = uri.clone();
            async move {
                let diags = self
                    .server_diagnostics(config, &uri, language_id, text)
                    .await?;
                Some(
                    diags
                        .into_iter()
                        .map(|d| SourcedDiagnostic {
                            server: config.name.clone(),
                            diagnostic: d,
                        })
                        .collect::<Vec<_>>(),
                )
            }
        }))
        .await;

        // `None` from a server means "couldn't answer" (not started /
        // mid-session death); `Some` means it answered (possibly clean). If NO
        // server answered there is nothing meaningful to attach — return
        // `None` so the tool stays silent rather than implying "checked,
        // clean" when really every server was down (`doc/lsp.md` §4.6).
        if results.iter().all(Option::is_none) {
            return None;
        }
        let mut out: Vec<SourcedDiagnostic> = results.into_iter().flatten().flatten().collect();
        // Deterministic order: by position, then server name.
        out.sort_by(|a, b| {
            (
                a.diagnostic.range.start.line,
                a.diagnostic.range.start.character,
            )
                .cmp(&(
                    b.diagnostic.range.start.line,
                    b.diagnostic.range.start.character,
                ))
                .then_with(|| a.server.cmp(&b.server))
        });
        Some(out)
    }

    /// Sync `text` to one shared server and collect its diagnostics, or `None`
    /// when the server can't answer (not started, mid-session death).
    /// Best-effort per server — a failure forfeits only this server's
    /// diagnostics. The document sync is serialized per-uri through the
    /// server's `doc_locks`, so concurrent sessions editing the same file keep
    /// its version monotonic (`doc/lsp.md` §5.2).
    async fn server_diagnostics(
        &self,
        config: &LspServerConfig,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Option<Vec<Diagnostic>> {
        let server = self.router.shared_server(&self.workspace, config).await;
        let client = self
            .router
            .get_or_spawn(&server, &self.workspace, &self.env_overlay)
            .await?;

        // Serialize this document's sync: `didOpen`/`didChange` to one uri
        // must keep its version monotonic across sessions.
        let doc_lock = server.doc_lock(uri).await;
        let doc_guard = doc_lock.lock().await;
        let Ok(version) = client.sync_document(uri, language_id, text).await else {
            // The cached client is broken — typically the server process died
            // mid-session. Drop it via the service (marking the failure +
            // flipping the lifecycle to `failed`) so a later op respawns
            // through the same single-spawn lock (§12 fail-loud).
            self.router.note_died(&server).await;
            return None;
        };
        let budget = Duration::from_millis(config.diag_timeout_ms);
        let diags = client.diagnostics(uri, version, budget).await;
        drop(doc_guard);
        // Answered (possibly "clean"): the (a1) ready signal that moves the
        // server from `starting` to `running`.
        self.router.note_answered(&server).await;
        Some(diags)
    }
}

/// The LSP `languageId` for a file extension. Servers dispatch on this id, and
/// strict ones (pyright et al.) reject documents whose id they don't know, so
/// well-known extension→id mismatches are mapped explicitly; anything else
/// falls back to the raw extension (which already equals the id for many
/// languages, e.g. `go`, `java`).
fn language_id_for(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" => "cpp",
        "rb" => "ruby",
        "php" => "php",
        other => other,
    }
}

/// Cap on diagnostics actually rendered (the assist's token budget —
/// `doc/lsp.md` §4): a file with hundreds of issues must not blow the tool
/// result's token budget on every touch. The true count still appears in the
/// header, and the tail is summarized rather than silently dropped.
const RENDER_CAP: usize = 20;

/// Render diagnostics as a compact block appended to a tool result.
///
/// Empty input yields an empty string (nothing to append). Positions are
/// converted from LSP's 0-based line/character to the 1-based `line:col` the
/// file tools and humans use. Over [`RENDER_CAP`], only the first `RENDER_CAP`
/// are listed; the header still reports the true count and a trailing line
/// names how many were omitted (with their line spans).
#[must_use]
pub fn render_diagnostics(path_label: &str, diagnostics: &[SourcedDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return String::new();
    }
    // A single-source language (the common case) omits the `via` tag to save
    // tokens; multi-source names each issue's server so the model can weigh
    // them (`doc/lsp.md` §5).
    let multi = diagnostics
        .first()
        .is_some_and(|f| diagnostics.iter().any(|d| d.server != f.server));
    let mut out = format!(
        "\n[diagnostics: {path_label}] {} issue(s)",
        diagnostics.len()
    );
    let mut outside: Vec<usize> = Vec::new();
    for sd in diagnostics.iter().take(RENDER_CAP) {
        let d = &sd.diagnostic;
        let line = d.range.start.line + 1;
        let col = d.range.start.character + 1;
        let severity = d.severity.map_or("diagnostic", DiagnosticSeverity::label);
        // Collapse multi-line messages to one line for a scannable block.
        let message = d.message.replace('\n', " ");
        let via = if multi {
            format!(" ({})", sd.server)
        } else {
            String::new()
        };
        let _ = write!(out, "\n  {line}:{col} {severity}{via}: {message}");
    }
    outside.extend(
        diagnostics
            .iter()
            .skip(RENDER_CAP)
            .map(|sd| sd.diagnostic.range.start.line as usize + 1),
    );
    if !outside.is_empty() {
        outside.sort_unstable();
        let _ = write!(
            out,
            "\n  … and {} more (lines {})",
            outside.len(),
            line_spans(&outside)
        );
    }
    out
}

/// Compact sorted 1-based lines into `a-b` spans: `[3,4,5,9]` → `"3-5, 9"`.
fn line_spans(lines: &[usize]) -> String {
    let mut spans: Vec<String> = Vec::new();
    let mut start = lines[0];
    let mut prev = lines[0];
    for &l in &lines[1..] {
        if l == prev + 1 {
            prev = l;
            continue;
        }
        spans.push(span(start, prev));
        start = l;
        prev = l;
    }
    spans.push(span(start, prev));
    spans.join(", ")
}

fn span(lo: usize, hi: usize) -> String {
    if lo == hi {
        lo.to_string()
    } else {
        format!("{lo}-{hi}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::*;
    use protocol::{Position, Range};

    fn cfg(extensions: &[&str]) -> LspConfig {
        LspConfig {
            servers: vec![LspServerConfig {
                name: "mock".to_owned(),
                command: "true".to_owned(),
                args: vec![],
                env: std::collections::HashMap::new(),
                extensions: extensions.iter().map(|s| (*s).to_owned()).collect(),
                enabled: true,
                diag_timeout_ms: 100,
                init_timeout_ms: 500,
            }],
        }
    }

    /// Wrap a bare [`Diagnostic`] with a server name for the render tests.
    fn sourced(server: &str, diagnostic: Diagnostic) -> SourcedDiagnostic {
        SourcedDiagnostic {
            server: server.to_owned(),
            diagnostic,
        }
    }

    fn manager(cfg: &LspConfig) -> Arc<LspManager> {
        let router: Arc<dyn LspRouter> = Arc::new(ProcessLspService::new());
        LspManager::new(router, cfg, PathBuf::from("/ws"), BTreeMap::new()).unwrap()
    }

    /// No configured servers → no manager at all, so callers pay nothing.
    #[test]
    fn empty_config_yields_no_manager() {
        let router: Arc<dyn LspRouter> = Arc::new(ProcessLspService::new());
        assert!(
            LspManager::new(
                router,
                &LspConfig::default(),
                PathBuf::from("/ws"),
                BTreeMap::new(),
            )
            .is_none()
        );
    }

    /// Well-known extension→languageId mismatches map to the canonical LSP id
    /// (strict servers reject documents with an id they don't know); an
    /// unmapped extension passes through unchanged.
    #[test]
    fn language_id_maps_known_extensions() {
        assert_eq!(language_id_for("rs"), "rust");
        assert_eq!(language_id_for("py"), "python");
        assert_eq!(language_id_for("ts"), "typescript");
        assert_eq!(language_id_for("toml"), "toml");
    }

    /// An unsupported extension returns `None` from `diagnostics` without ever
    /// starting a server — the zero-cost fast path.
    #[tokio::test]
    async fn unsupported_extension_is_none() {
        let m = manager(&cfg(&["rs"]));
        assert!(
            m.diagnostics(Path::new("/ws/a.py"), "print(1)")
                .await
                .is_none()
        );
    }

    /// The renderer converts 0-based LSP positions to 1-based and labels
    /// severity; an empty slice renders nothing.
    #[test]
    fn render_is_one_based_and_labeled() {
        assert_eq!(render_diagnostics("lib.rs", &[]), "");
        let diags = vec![sourced(
            "rust-analyzer",
            Diagnostic {
                range: Range {
                    start: Position {
                        line: 4,
                        character: 8,
                    },
                    end: Position {
                        line: 4,
                        character: 12,
                    },
                },
                severity: Some(DiagnosticSeverity::Error),
                message: "mismatched types".to_owned(),
            },
        )];
        let rendered = render_diagnostics("src/lib.rs", &diags);
        assert!(rendered.contains("[diagnostics: src/lib.rs] 1 issue(s)"));
        // Single-source: no `via` tag.
        assert!(rendered.contains("5:9 error: mismatched types"));
    }

    /// A multi-server language names each issue's source so the model can
    /// weigh them (`doc/lsp.md` §5).
    #[test]
    fn render_attributes_sources_when_multiple_servers() {
        let d = |line: u32| Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity: Some(DiagnosticSeverity::Error),
            message: "m".to_owned(),
        };
        let diags = vec![sourced("pyright", d(0)), sourced("ruff", d(1))];
        let rendered = render_diagnostics("a.py", &diags);
        assert!(rendered.contains("error (pyright): m"));
        assert!(rendered.contains("error (ruff): m"));
    }

    /// Over the render cap, only `RENDER_CAP` lines are emitted, the header
    /// still reports the TRUE count, and the omitted tail is summarized — so a
    /// file with hundreds of diagnostics cannot blow the tool result's token
    /// budget (`doc/lsp.md` §4).
    #[test]
    fn render_caps_long_lists_and_reports_true_total() {
        let one = |line: u32| Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity: Some(DiagnosticSeverity::Warning),
            message: format!("issue {line}"),
        };
        let diags: Vec<SourcedDiagnostic> = (0..30).map(|l| sourced("mock", one(l))).collect();
        let rendered = render_diagnostics("big.rs", &diags);
        assert!(rendered.contains("[diagnostics: big.rs] 30 issue(s)"));
        assert!(rendered.contains("… and 10 more (lines 21-30)"));
        // Exactly RENDER_CAP detail lines rendered (+ header + summary).
        assert_eq!(rendered.matches("warning:").count(), RENDER_CAP);
    }

    // --- end-to-end against a mock language server ---------------------------

    /// A mock stdio language server in Python. It speaks `Content-Length`
    /// framing, completes the `initialize` handshake, and whenever a document is
    /// opened or changed it publishes one diagnostic whose message echoes the
    /// document's first line — enough to prove the full connect → sync → publish
    /// → wait path without a real toolchain. It also sends a server→client
    /// `workspace/configuration` request right after initialize, so the test
    /// covers our auto-reply (the server would otherwise block).
    const MOCK_LSP: &str = r#"
import sys, json

def read_message():
    # Read headers until blank line, then the body of Content-Length bytes.
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode("ascii").strip()
        if line == "":
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    if length is None:
        return None
    body = sys.stdin.buffer.read(length)
    return json.loads(body)

def send(obj):
    body = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

def publish(uri, version, first_line):
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
        "uri": uri, "version": version,
        "diagnostics": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
            "severity": 1,
            "message": "echo: " + first_line,
        }],
    }})

while True:
    msg = read_message()
    if msg is None:
        break
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"positionEncoding": "utf-8"}}})
        # A server-initiated request; we must not block waiting for its reply.
        send({"jsonrpc": "2.0", "id": 9001, "method": "workspace/configuration", "params": {"items": []}})
    elif method == "textDocument/didOpen":
        doc = msg["params"]["textDocument"]
        first = doc["text"].splitlines()[0] if doc["text"] else ""
        publish(doc["uri"], doc.get("version", 1), first)
    elif method == "textDocument/didChange":
        p = msg["params"]
        uri = p["textDocument"]["uri"]
        version = p["textDocument"].get("version", 1)
        text = p["contentChanges"][0]["text"]
        first = text.splitlines()[0] if text else ""
        publish(uri, version, first)
"#;

    /// Write the mock server to `dir` and return a config routing `.rs` to it
    /// via `python3`.
    fn mock_lsp_config(dir: &std::path::Path) -> LspConfig {
        let script = dir.join("mock_lsp.py");
        std::fs::write(&script, MOCK_LSP).unwrap();
        LspConfig {
            servers: vec![LspServerConfig {
                name: "mock".to_owned(),
                command: "python3".to_owned(),
                args: vec![script.to_string_lossy().into_owned()],
                env: std::collections::HashMap::new(),
                extensions: vec!["rs".to_owned()],
                enabled: true,
                diag_timeout_ms: 2_000,
                init_timeout_ms: 4_000,
            }],
        }
    }

    /// Full round-trip: the manager lazily spawns the mock server, syncs a `.rs`
    /// document, and returns the diagnostic the server published — whose message
    /// echoes the file's first line, proving the text actually reached the
    /// server through `didOpen`. This is the load-bearing end-to-end test.
    #[tokio::test]
    async fn diagnostics_round_trip_through_mock_server() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let manager = LspManager::new(
            Arc::new(ProcessLspService::new()),
            &config,
            dir.path().to_path_buf(),
            BTreeMap::new(),
        )
        .unwrap();

        let file = dir.path().join("lib.rs");
        let diags = manager
            .diagnostics(&file, "fn broken( {\n")
            .await
            .expect("rs is supported, so Some");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].diagnostic.message, "echo: fn broken( {");
        assert_eq!(
            diags[0].diagnostic.severity,
            Some(DiagnosticSeverity::Error)
        );
        assert_eq!(diags[0].server, "mock");
    }

    /// A second op on the same file reuses the warm server (a `didChange`, not a
    /// respawn) and picks up the new first line — the version-gated wait returns
    /// the fresh publish, not the stale one.
    #[tokio::test]
    async fn warm_reuse_returns_fresh_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let manager = LspManager::new(
            Arc::new(ProcessLspService::new()),
            &config,
            dir.path().to_path_buf(),
            BTreeMap::new(),
        )
        .unwrap();
        let file = dir.path().join("lib.rs");

        let first = manager.diagnostics(&file, "first version\n").await.unwrap();
        assert_eq!(first[0].diagnostic.message, "echo: first version");

        let second = manager
            .diagnostics(&file, "second version\n")
            .await
            .unwrap();
        assert_eq!(second[0].diagnostic.message, "echo: second version");
    }

    /// `status()` lists only ACTIVATED servers: before any file op the mock
    /// `.rs` server is omitted (a rust+ts session never shows clangd/gopls);
    /// after a `diagnostics()` call spawns it, it appears as `running`
    /// (`doc/lsp.md` §5.1).
    #[tokio::test]
    async fn status_lists_only_activated_servers() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let manager = LspManager::new(
            Arc::new(ProcessLspService::new()),
            &config,
            dir.path().to_path_buf(),
            BTreeMap::new(),
        )
        .unwrap();

        // Untouched: nothing activated, so nothing listed.
        assert!(manager.status().await.is_empty());

        // Touch a `.rs` file: the server spawns and is now listed as running.
        let file = dir.path().join("lib.rs");
        manager.diagnostics(&file, "fn x() {}\n").await.unwrap();
        let status = manager.status().await;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "mock");
        assert_eq!(status[0].state, ServerState::Running);
    }

    // --- root-level sharing (`doc/lsp.md` §5.2) -------------------------------

    /// Two sessions (two `LspManager`s) over the SAME service and root share
    /// one server: the second manager's diagnostics ride the first's client,
    /// so `status()` lists exactly one server and both managers get answers.
    /// This is the whole point of the sharing — N sessions, one index.
    #[tokio::test]
    async fn sessions_under_one_root_share_a_server() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let service: Arc<dyn LspRouter> = Arc::new(ProcessLspService::new());
        let root = dir.path().to_path_buf();
        let mk = || {
            LspManager::new(Arc::clone(&service), &config, root.clone(), BTreeMap::new()).unwrap()
        };
        let session_a = mk();
        let session_b = mk();
        let file = dir.path().join("lib.rs");

        // Both sessions touch the same file. If they share one client, the
        // second's sync is a `didChange` on the already-open doc (version
        // keeps climbing); if each spawned its own, the service would still
        // report one key but the second would have re-`didOpen`ed. Either
        // way, both must get the echo, and the service lists ONE server.
        let da = session_a.diagnostics(&file, "fn a() {}\n").await.unwrap();
        let db = session_b.diagnostics(&file, "fn b() {}\n").await.unwrap();
        assert_eq!(da[0].diagnostic.message, "echo: fn a() {}");
        assert_eq!(db[0].diagnostic.message, "echo: fn b() {}");
        let status = service.status(&root).await;
        assert_eq!(status.len(), 1, "one shared server for the root");
        assert_eq!(status[0].name, "mock");
    }

    /// The sharing unit is the root, not the service: two DIFFERENT roots on
    /// one service get independent servers (the worktree abstraction — two
    /// worktrees of one workspace never share, `doc/lsp.md` §5.2).
    #[tokio::test]
    async fn different_roots_do_not_share() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir_a.path());
        let service: Arc<dyn LspRouter> = Arc::new(ProcessLspService::new());
        let ma = LspManager::new(
            Arc::clone(&service),
            &config,
            dir_a.path().to_path_buf(),
            BTreeMap::new(),
        )
        .unwrap();
        let mb = LspManager::new(
            Arc::clone(&service),
            &config,
            dir_b.path().to_path_buf(),
            BTreeMap::new(),
        )
        .unwrap();

        ma.diagnostics(&dir_a.path().join("a.rs"), "fn a() {}\n")
            .await
            .unwrap();
        // Root B untouched: its status is empty even though root A's server
        // is up — servers are keyed per root.
        assert_eq!(service.status(dir_a.path()).await.len(), 1);
        assert!(service.status(dir_b.path()).await.is_empty());

        mb.diagnostics(&dir_b.path().join("b.rs"), "fn b() {}\n")
            .await
            .unwrap();
        assert_eq!(service.status(dir_b.path()).await.len(), 1);
    }

    /// The `starting` transient (`doc/lsp.md` §5.2): a freshly spawned server
    /// that has not yet answered diagnostics reports `starting`; the first
    /// successful answer flips it to `running` (heuristic a1). This is what
    /// the input-area "indexing…" indicator reads.
    #[tokio::test]
    async fn starting_until_first_answer_then_running() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let service: Arc<dyn LspRouter> = Arc::new(ProcessLspService::new());
        let root = dir.path().to_path_buf();
        let server = service.shared_server(&root, &config.servers[0]).await;

        // Spawned + handshook but never answered → `starting`.
        let _client = service
            .get_or_spawn(&server, &root, &BTreeMap::new())
            .await
            .expect("mock server spawns");
        let status = service.status(&root).await;
        assert_eq!(status[0].state, ServerState::Starting);

        // The (a1) ready signal: first successful answer → `running`.
        service.note_answered(&server).await;
        let status = service.status(&root).await;
        assert_eq!(status[0].state, ServerState::Running);
    }

    // --- lifecycle: reclaim + idle doc close (`doc/lsp.md` §5.2) ------------

    /// A root with no active session whose grace has elapsed is reclaimed
    /// (its server dropped from the map, killing the subprocess); an active
    /// root's server is never reclaimed, and a root inside its grace period is
    /// kept. This is the structural-event reclaim — never server idleness.
    #[tokio::test]
    async fn reclaim_respects_activity_and_grace() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let root = dir.path().to_path_buf();

        // Zero grace: any inactive root is immediately reclaimable.
        let concrete = Arc::new(
            ProcessLspService::new()
                .with_periods(std::time::Duration::ZERO, DEFAULT_DOC_IDLE_CLOSE),
        );
        // `concrete` kept for state assertions (`server_count` is not on the trait).
        let service: Arc<dyn LspRouter> = concrete.clone();
        let m =
            LspManager::new(Arc::clone(&service), &config, root.clone(), BTreeMap::new()).unwrap();
        m.diagnostics(&dir.path().join("lib.rs"), "fn x() {}\n")
            .await
            .unwrap();
        assert_eq!(concrete.server_count().await, 1);

        // Active root → kept even with zero grace.
        let active = std::collections::HashSet::from([root.clone()]);
        assert_eq!(service.reclaim_inactive(&active).await, 0);
        assert_eq!(concrete.server_count().await, 1);

        // Inactive root + grace elapsed → reclaimed.
        let empty = std::collections::HashSet::new();
        assert_eq!(service.reclaim_inactive(&empty).await, 1);
        assert_eq!(concrete.server_count().await, 0);
    }

    /// A root reclaimed inside its grace period is kept: with a LONG grace,
    /// even an inactive root survives — the grace is what protects a user who
    /// tabs away and back from a full re-index.
    #[tokio::test]
    async fn grace_period_protects_recently_idle_root() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let root = dir.path().to_path_buf();
        let concrete = Arc::new(ProcessLspService::new()); // default 30-min grace
        // `concrete` kept for state assertions (`server_count` is not on the trait).
        let service: Arc<dyn LspRouter> = concrete.clone();
        let m =
            LspManager::new(Arc::clone(&service), &config, root.clone(), BTreeMap::new()).unwrap();
        m.diagnostics(&dir.path().join("lib.rs"), "fn x() {}\n")
            .await
            .unwrap();
        assert_eq!(concrete.server_count().await, 1);
        // Inactive but well within grace → kept.
        let empty = std::collections::HashSet::new();
        assert_eq!(service.reclaim_inactive(&empty).await, 0);
        assert_eq!(concrete.server_count().await, 1);
    }

    /// An open document idle past the period is `didClose`d (freeing the
    /// server's per-doc memory), and the next touch transparently re-opens it
    /// — the workspace index stays warm, only the cached open copy cycles
    /// (`doc/lsp.md` §5.2). Zero idle-period closes everything immediately.
    #[tokio::test]
    async fn idle_doc_close_reopens_on_next_touch() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let root = dir.path().to_path_buf();
        // Zero idle-close period: every open doc is immediately closeable.
        let service: Arc<dyn LspRouter> = Arc::new(
            ProcessLspService::new().with_periods(DEFAULT_RECLAIM_GRACE, std::time::Duration::ZERO),
        );
        let m =
            LspManager::new(Arc::clone(&service), &config, root.clone(), BTreeMap::new()).unwrap();
        let file = dir.path().join("lib.rs");
        m.diagnostics(&file, "fn x() {}\n").await.unwrap();

        // The doc was opened, so a zero-idle sweep closes exactly one.
        assert_eq!(service.close_idle_documents().await, 1);
        // Already closed → a second sweep closes nothing.
        assert_eq!(service.close_idle_documents().await, 0);

        // The next touch re-opens it (transparently) and still returns the
        // echo — the server (and its index) never went away.
        let diags = m.diagnostics(&file, "fn y() {}\n").await.unwrap();
        assert_eq!(diags[0].diagnostic.message, "echo: fn y() {}");
    }

    /// A server whose command does not exist never fails the op: `diagnostics`
    /// returns `None` (nothing to attach), and the file op that called it is
    /// free to succeed.
    #[tokio::test]
    async fn broken_server_yields_none_not_error() {
        let config = LspConfig {
            servers: vec![LspServerConfig {
                name: "nope".to_owned(),
                command: "definitely-not-a-real-lsp-xyz".to_owned(),
                args: vec![],
                env: std::collections::HashMap::new(),
                extensions: vec!["rs".to_owned()],
                enabled: true,
                diag_timeout_ms: 200,
                init_timeout_ms: 500,
            }],
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(
            Arc::new(ProcessLspService::new()),
            &config,
            dir.path().to_path_buf(),
            BTreeMap::new(),
        )
        .unwrap();
        let file = dir.path().join("lib.rs");
        assert!(manager.diagnostics(&file, "x\n").await.is_none());
    }

    /// Integration through the `write` tool: writing a `.rs` file with the LSP
    /// assist attached appends the server's diagnostics block to the tool
    /// result; the same tool built without the assist returns only the write
    /// summary. This pins the whole feature at its actual seam — the tool
    /// output the model sees.
    #[tokio::test]
    async fn write_tool_appends_diagnostics_when_lsp_attached() {
        use crate::core::payload::Content;
        use crate::tool::{Tool, ToolInput, WriteTool};

        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let manager = LspManager::new(
            Arc::new(ProcessLspService::new()),
            &config,
            dir.path().to_path_buf(),
            BTreeMap::new(),
        )
        .unwrap();

        let input = || ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "path": "lib.rs", "content": "fn oops( {\n" }),
            timeout: std::time::Duration::from_secs(5),
            progress: None,
        };
        let text = |out: &crate::core::payload::ToolOutput| {
            out.content
                .iter()
                .map(|c| match c {
                    Content::Text(t) => t.clone(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // With the assist: the diagnostics block rides along.
        let with = WriteTool::new(dir.path().to_path_buf()).with_lsp(Some(manager));
        let out = with.invoke(input()).await.unwrap();
        assert!(!out.is_error);
        let body = text(&out);
        assert!(body.contains("wrote lib.rs"), "keeps the write summary");
        assert!(
            body.contains("[diagnostics: lib.rs]") && body.contains("echo: fn oops( {"),
            "appends the server diagnostics: {body}"
        );

        // Without the assist: only the write summary, no diagnostics section.
        let without = WriteTool::new(dir.path().to_path_buf());
        let out = without.invoke(input()).await.unwrap();
        assert!(!text(&out).contains("[diagnostics:"));
    }
}
