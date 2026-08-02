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
mod config;
mod protocol;

pub use client::{LspClient, LspError, uri_for};
pub use config::{ConfigError, LspConfig, LspServerConfig};
pub use protocol::{Diagnostic, DiagnosticSeverity};

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

/// One configured language server plus its lazily-started client.
struct Server {
    config: LspServerConfig,
    /// `None` until the first file of this language is touched; then holds the
    /// live client (or stays `None` if the last start attempt failed — a broken
    /// server must never fail a file op, only forfeit diagnostics).
    client: Mutex<Option<Arc<LspClient>>>,
    /// When the last start attempt failed, if any. Guards a broken server from
    /// re-paying the full `init_timeout` on *every* file op: within
    /// [`START_RETRY_COOLDOWN`] of a failure we skip the retry outright. Reset
    /// implicitly on success (a live `client` short-circuits before this is
    /// read).
    last_failed_at: Mutex<Option<std::time::Instant>>,
}

/// How long to wait after a failed start before trying to spawn a server again.
/// A misconfigured or crashing language server would otherwise stall every
/// single file op by `init_timeout_ms`; this bounds the damage to one attempt
/// per window.
const START_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

/// Owns every language server for a session.
///
/// Routes a file op's diagnostics request to the right one. Held alive for the
/// session's lifetime (like `mcp_clients`); dropping it kills the server
/// subprocesses.
pub struct LspManager {
    workspace: PathBuf,
    env_overlay: BTreeMap<String, Option<String>>,
    servers: Vec<Server>,
}

impl LspManager {
    /// Build a manager for `config`, rooted at `workspace`. Spawns nothing yet —
    /// servers start lazily on first touch. Returns `None` when no servers are
    /// configured, so callers can skip the assist entirely with zero overhead.
    ///
    /// Configured servers are logged at setup (mirrors
    /// [`crate::mcp::connect_all`]) so a present-but-bad `lsp.toml` is at least
    /// visible; per-start failures surface later via `tracing` (§12 fail-loud).
    #[must_use]
    pub fn new(
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
        let servers = config
            .servers
            .iter()
            .cloned()
            .map(|config| Server {
                config,
                client: Mutex::new(None),
                last_failed_at: Mutex::new(None),
            })
            .collect();
        Some(Arc::new(Self {
            workspace,
            env_overlay,
            servers,
        }))
    }

    /// The index of the server that handles `path`'s extension, if any. First
    /// match wins (config order).
    fn server_for(&self, path: &Path) -> Option<usize> {
        let ext = path.extension()?.to_str()?;
        self.servers
            .iter()
            .position(|s| s.config.extensions.iter().any(|e| e == ext))
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
    pub async fn diagnostics(&self, abs_path: &Path, text: &str) -> Option<Vec<Diagnostic>> {
        let idx = self.server_for(abs_path)?;
        let server = &self.servers[idx];

        let client = self.ensure_started(server).await?;

        let uri = uri_for(abs_path);
        let language_id = abs_path
            .extension()
            .and_then(|e| e.to_str())
            .map_or("", language_id_for);

        let version = match client.sync_document(&uri, language_id, text).await {
            Ok(version) => version,
            Err(e) => {
                // The cached client is broken — typically the server process
                // died mid-session. Drop it and mark the failure so a later op
                // respawns through the same cooldown that guards start
                // failures, and log instead of the assist silently vanishing
                // for the rest of the session (§12 fail-loud).
                *server.client.lock().await = None;
                *server.last_failed_at.lock().await = Some(std::time::Instant::now());
                tracing::warn!(
                    server = %server.config.name,
                    cooldown_secs = START_RETRY_COOLDOWN.as_secs(),
                    "lsp: server stopped responding ({e}); client dropped, diagnostics disabled"
                );
                return None;
            }
        };
        let budget = Duration::from_millis(server.config.diag_timeout_ms);
        Some(client.diagnostics(&uri, version, budget).await)
    }

    /// Return the server's live client, starting it on first use. A failed start
    /// forfeits diagnostics for this op (returns `None`) but never fails the op;
    /// it is reported once on stderr and then suppressed for
    /// [`START_RETRY_COOLDOWN`] so a broken server does not re-pay the full
    /// `init_timeout` on every subsequent file op. The `client` `Mutex` guard
    /// means only one op spawns the server even under concurrent touches.
    //
    // The guard is deliberately held across `connect().await`: that is what
    // serializes concurrent first-touches so exactly one spawns the server.
    #[allow(clippy::significant_drop_tightening)]
    async fn ensure_started(&self, server: &Server) -> Option<Arc<LspClient>> {
        let mut guard = server.client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Some(Arc::clone(client));
        }
        // Recently failed? Skip the retry (and its init_timeout stall) until the
        // cooldown elapses.
        {
            let last = server.last_failed_at.lock().await;
            if last.is_some_and(|t| t.elapsed() < START_RETRY_COOLDOWN) {
                return None;
            }
        }
        let init_timeout = Duration::from_millis(server.config.init_timeout_ms);
        match LspClient::connect(
            &server.config,
            &self.workspace,
            &self.env_overlay,
            init_timeout,
        )
        .await
        {
            Ok(client) => {
                let client = Arc::new(client);
                *guard = Some(Arc::clone(&client));
                Some(client)
            }
            Err(e) => {
                *server.last_failed_at.lock().await = Some(std::time::Instant::now());
                // A misconfigured server must not fail 100% silently (§12).
                tracing::warn!(
                    server = %server.config.name,
                    cooldown_secs = START_RETRY_COOLDOWN.as_secs(),
                    "lsp: server failed to start ({e}); diagnostics disabled"
                );
                None
            }
        }
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
pub fn render_diagnostics(path_label: &str, diagnostics: &[Diagnostic]) -> String {
    if diagnostics.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n[diagnostics: {path_label}] {} issue(s)",
        diagnostics.len()
    );
    let mut outside: Vec<usize> = Vec::new();
    for d in diagnostics.iter().take(RENDER_CAP) {
        let line = d.range.start.line + 1;
        let col = d.range.start.character + 1;
        let severity = d.severity.map_or("diagnostic", DiagnosticSeverity::label);
        // Collapse multi-line messages to one line for a scannable block.
        let message = d.message.replace('\n', " ");
        let _ = write!(out, "\n  {line}:{col} {severity}: {message}");
    }
    outside.extend(
        diagnostics
            .iter()
            .skip(RENDER_CAP)
            .map(|d| d.range.start.line as usize + 1),
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
                diag_timeout_ms: 100,
                init_timeout_ms: 500,
            }],
        }
    }

    fn manager(cfg: &LspConfig) -> Arc<LspManager> {
        LspManager::new(cfg, PathBuf::from("/ws"), BTreeMap::new()).unwrap()
    }

    /// No configured servers → no manager at all, so callers pay nothing.
    #[test]
    fn empty_config_yields_no_manager() {
        assert!(
            LspManager::new(&LspConfig::default(), PathBuf::from("/ws"), BTreeMap::new()).is_none()
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
        let diags = vec![Diagnostic {
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
        }];
        let rendered = render_diagnostics("src/lib.rs", &diags);
        assert!(rendered.contains("[diagnostics: src/lib.rs] 1 issue(s)"));
        assert!(rendered.contains("5:9 error: mismatched types"));
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
        let diags: Vec<Diagnostic> = (0..30).map(one).collect();
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
        let manager = LspManager::new(&config, dir.path().to_path_buf(), BTreeMap::new()).unwrap();

        let file = dir.path().join("lib.rs");
        let diags = manager
            .diagnostics(&file, "fn broken( {\n")
            .await
            .expect("rs is supported, so Some");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "echo: fn broken( {");
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::Error));
    }

    /// A second op on the same file reuses the warm server (a `didChange`, not a
    /// respawn) and picks up the new first line — the version-gated wait returns
    /// the fresh publish, not the stale one.
    #[tokio::test]
    async fn warm_reuse_returns_fresh_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let config = mock_lsp_config(dir.path());
        let manager = LspManager::new(&config, dir.path().to_path_buf(), BTreeMap::new()).unwrap();
        let file = dir.path().join("lib.rs");

        let first = manager.diagnostics(&file, "first version\n").await.unwrap();
        assert_eq!(first[0].message, "echo: first version");

        let second = manager
            .diagnostics(&file, "second version\n")
            .await
            .unwrap();
        assert_eq!(second[0].message, "echo: second version");
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
                diag_timeout_ms: 200,
                init_timeout_ms: 500,
            }],
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(&config, dir.path().to_path_buf(), BTreeMap::new()).unwrap();
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
        let manager = LspManager::new(&config, dir.path().to_path_buf(), BTreeMap::new()).unwrap();

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
