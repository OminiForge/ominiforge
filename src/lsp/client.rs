//! The stdio LSP client: owns one language-server subprocess, frames JSON-RPC
//! over its stdin/stdout using `Content-Length` headers, and runs a background
//! reader that demultiplexes the three inbound message kinds.
//!
//! Why a background reader (unlike [`crate::mcp::client`], which reads inline
//! right after each write): an LSP server pushes `publishDiagnostics` whenever
//! it finishes analyzing a file — unsolicited, at any time — and occasionally
//! sends *us* requests it wants answered. A synchronous "write then read my
//! reply" loop would drop those or deadlock. So the reader owns the stdout
//! stream for the client's whole life; our request/response correlation goes
//! through a shared id→oneshot map, diagnostics land in a shared per-uri map,
//! and server→client requests get an empty reply so the server never stalls.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, Notify, oneshot};

use super::config::LspServerConfig;
use super::protocol::{
    Diagnostic, Incoming, InitializeResult, Notification, OutgoingResponse,
    PublishDiagnosticsParams, Request,
};
use crate::process_env::apply_env_overlay;

/// Why an LSP operation failed. Diagnostics are best-effort, so most call sites
/// log-and-continue rather than surface these to the model.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to spawn lsp server `{0}`: {1}")]
    Spawn(String, String),
    #[error("lsp server closed the connection")]
    Closed,
    #[error("lsp io error: {0}")]
    Io(String),
    #[error("lsp server returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("lsp initialize timed out")]
    InitTimeout,
}

/// Diagnostics for one open document plus a version stamp, so a waiter can tell
/// a fresh publish (version ≥ the edit it made) from a stale one.
#[derive(Debug, Clone, Default)]
struct DiagEntry {
    diagnostics: Vec<Diagnostic>,
    /// The `version` the server attributed the diagnostics to, if any.
    version: Option<i64>,
    /// A monotonically bumped counter, incremented on **every** publish for
    /// this uri (even when the server omits a version). Lets a waiter detect
    /// "a new publish arrived since I started waiting" without relying on the
    /// optional server version.
    seq: u64,
}

/// Shared state the background reader writes and callers read.
///
/// `stdin` lives here (not just on [`LspClient`]) because the reader must be
/// able to answer requests the server sends *us* — it holds an `Arc<Shared>`,
/// so putting the write half here lets it reply without a separate channel.
struct Shared {
    /// The server's framed stdin. Behind a mutex: writers (our requests, the
    /// reader's replies) must not interleave bytes on one stream. Boxed as a
    /// `dyn` writer so tests can substitute a sink for the real `ChildStdin`.
    stdin: Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
    /// Pending responses to requests we sent, keyed by our request id.
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, LspError>>>>,
    /// Latest diagnostics per document uri.
    diagnostics: Mutex<HashMap<String, DiagEntry>>,
    /// Woken on every diagnostics publish, so waiters re-check their uri.
    published: Notify,
}

impl Shared {
    /// Serialize `value` and write it with an LSP `Content-Length` frame. The
    /// one write path for both our requests and the reader's replies.
    async fn write_message<T: serde::Serialize + Sync>(&self, value: &T) -> Result<(), LspError> {
        let body = serde_json::to_vec(value).map_err(|e| LspError::Io(e.to_string()))?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| LspError::Io(e.to_string()))?;
        stdin
            .write_all(&body)
            .await
            .map_err(|e| LspError::Io(e.to_string()))?;
        stdin.flush().await.map_err(|e| LspError::Io(e.to_string()))
    }

    /// A `Shared` whose stdin is a discarding sink — for unit-testing [`route`]
    /// without a real subprocess.
    #[cfg(test)]
    fn for_test() -> Self {
        Self {
            stdin: Mutex::new(Box::new(tokio::io::sink())),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            published: Notify::new(),
        }
    }
}

/// A connected language server.
///
/// Owns the subprocess, its shared state (incl. framed stdin), and the reader
/// task. The subprocess is killed on drop (`kill_on_drop`). No `Debug` — the
/// boxed stdin writer in `Shared` isn't `Debug`, and nothing needs to print a
/// client.
pub struct LspClient {
    next_id: AtomicU64,
    shared: Arc<Shared>,
    /// Per-uri document version, bumped on each `didChange` we send.
    versions: Mutex<HashMap<String, i64>>,
    /// Per-uri last-touch time, for idle document closing (`doc/lsp.md` §5.2):
    /// an open doc the server holds in memory is `didClose`d after it sits
    /// untouched, freeing the server's per-doc text/syntax tree while the
    /// workspace index (the server's real asset) stays warm.
    doc_last_used: Mutex<HashMap<String, std::time::Instant>>,
    /// The position encoding the server negotiated (`utf-8` if it honored our
    /// request, else `utf-16`). Recorded for future position-mapping ops; the
    /// diagnostics assist renders the server's own line numbers directly, so it
    /// does not yet consume this.
    #[allow(dead_code)]
    position_encoding: String,
    _child: Child,
    _reader: tokio::task::JoinHandle<()>,
}

impl LspClient {
    /// Spawn `server` rooted at `workspace`, run the `initialize` handshake
    /// (bounded by `init_timeout`), and return the live client. Full workspace
    /// indexing continues in the background after this returns.
    ///
    /// # Errors
    /// [`LspError`] if the server fails to spawn or the handshake errors/times
    /// out.
    pub async fn connect(
        server: &LspServerConfig,
        workspace: &Path,
        env_overlay: &std::collections::BTreeMap<String, Option<String>>,
        init_timeout: std::time::Duration,
    ) -> Result<Self, LspError> {
        let mut command = tokio::process::Command::new(&server.command);
        command.args(&server.args);
        apply_env_overlay(&mut command, env_overlay);
        let mut child = command
            .envs(&server.env)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LspError::Spawn(server.name.clone(), e.to_string()))?;

        let stdin = child.stdin.take().ok_or(LspError::Closed)?;
        let stdout = child.stdout.take().ok_or(LspError::Closed)?;

        let shared = Arc::new(Shared {
            stdin: Mutex::new(Box::new(stdin)),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            published: Notify::new(),
        });
        let reader = tokio::spawn(read_loop(BufReader::new(stdout), Arc::clone(&shared)));

        let mut client = Self {
            next_id: AtomicU64::new(1),
            shared,
            versions: Mutex::new(HashMap::new()),
            doc_last_used: Mutex::new(HashMap::new()),
            position_encoding: "utf-16".to_owned(),
            _child: child,
            _reader: reader,
        };

        let encoding = tokio::time::timeout(init_timeout, client.initialize(workspace))
            .await
            .map_err(|_| LspError::InitTimeout)??;
        client.position_encoding = encoding;
        Ok(client)
    }

    /// The `initialize` request + `initialized` notification. Returns the
    /// negotiated position encoding.
    async fn initialize(&self, workspace: &Path) -> Result<String, LspError> {
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": path_to_uri(workspace),
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "versionSupport": true },
                    "synchronization": { "didSave": false }
                },
                "workspace": {}
            },
            // Ask for utf-8 to sidestep UTF-16 offset math; a server that
            // ignores this falls back to its mandated utf-16 default.
            "general": { "positionEncodings": ["utf-8"] },
        });
        let result = self.request("initialize", Some(params)).await?;
        let parsed: InitializeResult = serde_json::from_value(result).unwrap_or_default();
        let encoding = parsed
            .capabilities
            .position_encoding
            .unwrap_or_else(|| "utf-16".to_owned());

        self.notify("initialized", Some(serde_json::json!({})))
            .await?;
        Ok(encoding)
    }

    /// Open a document with its current text (LSP `textDocument/didOpen`), or —
    /// if already open — push its new text as a full-sync `didChange`. Returns
    /// the version stamped on the change, so the caller can wait for
    /// diagnostics computed at or after it.
    ///
    /// # Errors
    /// [`LspError::Io`]/[`LspError::Closed`] if the notification cannot be
    /// written (the server closed its stdin).
    pub async fn sync_document(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Result<i64, LspError> {
        let mut versions = self.versions.lock().await;
        let already_open = versions.contains_key(uri);
        let version = versions.entry(uri.to_owned()).or_insert(0);
        *version += 1;
        let version = *version;
        drop(versions);
        // A touch refreshes the doc's idle clock (drives idle-close, §5.2).
        self.doc_last_used
            .lock()
            .await
            .insert(uri.to_owned(), std::time::Instant::now());

        if already_open {
            self.notify(
                "textDocument/didChange",
                Some(serde_json::json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }],
                })),
            )
            .await?;
        } else {
            self.notify(
                "textDocument/didOpen",
                Some(serde_json::json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }
                })),
            )
            .await?;
        }
        Ok(version)
    }

    /// `didClose` every open document that has sat untouched for longer than
    /// `max_idle`, freeing the server's per-document memory (`doc/lsp.md`
    /// §5.2). The workspace index is untouched — only the cached open copy is
    /// released, and the next touch re-`didOpen`s it transparently. Returns
    /// the number of documents closed.
    pub async fn close_idle_docs(&self, max_idle: std::time::Duration) -> usize {
        let now = std::time::Instant::now();
        let stale: Vec<String> = self
            .doc_last_used
            .lock()
            .await
            .iter()
            .filter(|(_, t)| now.duration_since(**t) > max_idle)
            .map(|(uri, _)| uri.clone())
            .collect();
        let mut closed = 0usize;
        for uri in stale {
            // A doc that was never opened (already closed, or only ever
            // diagnosed) has no `didClose` to send.
            if self.versions.lock().await.remove(&uri).is_none() {
                continue;
            }
            self.doc_last_used.lock().await.remove(&uri);
            self.shared.diagnostics.lock().await.remove(&uri);
            if self
                .notify(
                    "textDocument/didClose",
                    Some(serde_json::json!({ "textDocument": { "uri": uri } })),
                )
                .await
                .is_ok()
            {
                closed += 1;
            }
        }
        closed
    }

    /// Wait up to `budget` for diagnostics on `uri` that are *fresh* — i.e. a
    /// publish observed after `after_seq` (the seq captured before we synced),
    /// or one whose server version is ≥ `min_version`. Returns the freshest
    /// diagnostics seen, or whatever is cached when the budget elapses (which
    /// may be an empty list — a clean file — or nothing yet).
    pub async fn diagnostics(
        &self,
        uri: &str,
        min_version: i64,
        budget: std::time::Duration,
    ) -> Vec<Diagnostic> {
        let after_seq = self
            .shared
            .diagnostics
            .lock()
            .await
            .get(uri)
            .map_or(0, |e| e.seq);

        let deadline = tokio::time::Instant::now() + budget;
        loop {
            // Register for the next publish *before* checking the map. `Notify`
            // only arms a waiter when the `notified()` future is first polled,
            // not when it is created — so we pin it and `enable()` it up front,
            // guaranteeing a `notify_waiters()` that fires between our map check
            // and the `.await` below is still delivered (not lost in the gap).
            let notified = self.shared.published.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(entry) = self.shared.diagnostics.lock().await.get(uri) {
                let fresh_by_seq = entry.seq > after_seq;
                let fresh_by_version = matches!(
                    (entry.version, min_version),
                    (Some(v), min) if v >= min
                );
                if fresh_by_seq || fresh_by_version {
                    return entry.diagnostics.clone();
                }
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero()
                || tokio::time::timeout(remaining, notified.as_mut())
                    .await
                    .is_err()
            {
                // Budget spent: return whatever is cached (possibly empty).
                return self
                    .shared
                    .diagnostics
                    .lock()
                    .await
                    .get(uri)
                    .map(|e| e.diagnostics.clone())
                    .unwrap_or_default();
            }
        }
    }

    /// Send a JSON-RPC request and await its response through the reader's
    /// oneshot channel, up to no explicit bound here (callers wrap in a
    /// timeout). Used by [`Self::initialize`] and, later, request-based tools.
    ///
    /// # Errors
    /// [`LspError::Rpc`] if the server returns an error object;
    /// [`LspError::Closed`] if the connection drops before a reply arrives.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().await.insert(id, tx);
        self.shared
            .write_message(&Request::new(id, method, params))
            .await?;
        rx.await.unwrap_or(Err(LspError::Closed))
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), LspError> {
        self.shared
            .write_message(&Notification::new(method, params))
            .await
    }
}

/// The background reader: frame-decode stdout forever, routing each message to
/// the right shared slot. Exits when the pipe closes (server gone), then drains
/// the pending map so a waiter blocked in `request()` resolves immediately with
/// [`LspError::Closed`] instead of hanging until its caller-side timeout.
async fn read_loop<R>(mut stdout: BufReader<R>, shared: Arc<Shared>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    while let Some(body) = read_frame(&mut stdout).await {
        let Ok(msg) = serde_json::from_slice::<Incoming>(&body) else {
            continue; // Unparseable line — skip, keep the stream alive.
        };
        route(&shared, msg).await;
    }
    // The pipe closed: the server will never answer again. Dropping each
    // oneshot sender fails its receiver right away.
    shared.pending.lock().await.clear();
}

/// Dispatch one decoded inbound message by kind (see [`Incoming`]'s table).
///
/// Classification is structural and `method`-first, because ids are not a
/// reliable discriminator: a server-initiated request may carry a string id,
/// and its numeric id lives in a different space from ours. So `method`
/// present ⇒ request-or-notification (tell apart by `id` presence); `method`
/// absent ⇒ a response to one of our requests (correlate by our u64 id).
async fn route(shared: &Arc<Shared>, msg: Incoming) {
    if let Some(method) = msg.method.as_deref() {
        if msg.id.is_some() {
            // A request from the server (e.g. `workspace/configuration`,
            // `client/registerCapability`, `window/workDoneProgress/create`).
            // We implement none of them, but the server may block waiting for a
            // reply, so we answer every one with an empty result of the right
            // shape (see `empty_result_for`).
            if let Some(id) = msg.id {
                let result = empty_result_for(method, msg.params.as_ref());
                let _ = shared
                    .write_message(&OutgoingResponse::with_result(id, result))
                    .await;
            }
            return;
        }
        // A notification from the server.
        if method == "textDocument/publishDiagnostics"
            && let Some(params) = msg
                .params
                .and_then(|p| serde_json::from_value::<PublishDiagnosticsParams>(p).ok())
        {
            let mut map = shared.diagnostics.lock().await;
            let entry = map.entry(params.uri).or_default();
            entry.diagnostics = params.diagnostics;
            entry.version = params.version;
            entry.seq += 1;
            drop(map);
            shared.published.notify_waiters();
        }
        // Every other notification is ignored.
        return;
    }

    // No method → a response to one of our requests; correlate by our u64 id.
    if let Some(id) = msg.our_request_id()
        && let Some(tx) = shared.pending.lock().await.remove(&id)
    {
        let outcome = match msg.error {
            Some(err) => Err(LspError::Rpc {
                code: err.code,
                message: err.message,
            }),
            None => Ok(msg.result.unwrap_or(serde_json::Value::Null)),
        };
        let _ = tx.send(outcome);
    }
}

/// The empty `result` payload for a server-initiated request we don't
/// implement. `workspace/configuration` is answered with one `null` per
/// requested item — the spec'd response shape, which strict servers (pyright)
/// require; a bare `null` there can break the server's configuration handling.
/// Every other request gets `result: null`, just enough to unblock the server.
fn empty_result_for(method: &str, params: Option<&serde_json::Value>) -> serde_json::Value {
    if method != "workspace/configuration" {
        return serde_json::Value::Null;
    }
    let count = params
        .and_then(|p| p.get("items"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    serde_json::Value::Array(vec![serde_json::Value::Null; count])
}

/// Read one `Content-Length`-framed message body from `stdout`. Returns `None`
/// on EOF or a malformed/again-unrecoverable header (treated as connection
/// end).
async fn read_frame<R>(stdout: &mut BufReader<R>) -> Option<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Parse headers line by line until the blank separator; we only need
    // Content-Length. Header lines are CRLF-terminated ASCII.
    let mut content_length: Option<usize> = None;
    loop {
        let line = read_header_line(stdout).await?;
        if line.is_empty() {
            break; // blank line: headers done
        }
        if let Some(rest) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = rest.trim().parse::<usize>().ok();
        }
        // Other headers (Content-Type) are ignored.
    }
    let len = content_length?;
    let mut body = vec![0u8; len];
    stdout.read_exact(&mut body).await.ok()?;
    Some(body)
}

/// Read a single CRLF-terminated header line, returning it without the
/// trailing `\r\n`. `None` on EOF.
async fn read_header_line<R>(stdout: &mut BufReader<R>) -> Option<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let n = stdout.read_line(&mut line).await.ok()?;
    if n == 0 {
        return None;
    }
    Some(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// Render a filesystem path as a `file://` URI. Minimal: absolute paths only
/// (workspace paths always are), percent-encoding just the characters that
/// would otherwise break the URI. rust-analyzer and friends accept this form.
fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(byte as char);
            }
            _ => {
                let _ = write!(uri, "%{byte:02X}");
            }
        }
    }
    uri
}

/// Public wrapper so the manager and tools build the same uri form the client
/// sends the server.
#[must_use]
pub fn uri_for(path: &Path) -> String {
    path_to_uri(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::significant_drop_tightening)]

    use super::*;
    use std::io::Cursor;

    /// A framed message round-trips: header + body decodes back to the body
    /// bytes, and a second frame in the same buffer is read independently.
    #[tokio::test]
    async fn read_frame_decodes_two_messages() {
        let raw = b"Content-Length: 2\r\n\r\n{}Content-Length: 14\r\n\r\n{\"hello\":true}";
        let mut buf = BufReader::new(Cursor::new(raw.to_vec()));

        let first = read_frame(&mut buf).await.unwrap();
        assert_eq!(first, b"{}");
        let second = read_frame(&mut buf).await.unwrap();
        assert_eq!(second, br#"{"hello":true}"#);
        assert!(read_frame(&mut buf).await.is_none());
    }

    /// The reader routes a `publishDiagnostics` notification into the shared
    /// per-uri map and wakes waiters (seq bumped from 0 to 1).
    #[tokio::test]
    async fn route_stores_published_diagnostics() {
        let shared = Arc::new(Shared::for_test());
        let msg: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{
                "uri":"file:///w/lib.rs","version":3,
                "diagnostics":[{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":4}},"severity":1,"message":"boom"}]
            }}"#,
        ).unwrap();
        route(&shared, msg).await;

        let map = shared.diagnostics.lock().await;
        let entry = map.get("file:///w/lib.rs").unwrap();
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.version, Some(3));
        assert_eq!(entry.diagnostics.len(), 1);
        assert_eq!(entry.diagnostics[0].message, "boom");
    }

    /// A response routes to the matching pending oneshot by our request id.
    #[tokio::test]
    async fn route_delivers_response_to_pending_request() {
        let shared = Arc::new(Shared::for_test());
        let (tx, rx) = oneshot::channel();
        shared.pending.lock().await.insert(42, tx);

        let msg: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":42,"result":{"ok":true}}"#).unwrap();
        route(&shared, msg).await;

        let got = rx.await.unwrap().unwrap();
        assert_eq!(got, serde_json::json!({"ok": true}));
    }

    /// An absolute path with a space becomes a percent-encoded `file://` URI.
    #[test]
    fn path_to_uri_percent_encodes() {
        let uri = path_to_uri(Path::new("/home/u/my project/lib.rs"));
        assert_eq!(uri, "file:///home/u/my%20project/lib.rs");
    }

    /// `workspace/configuration` gets one null per requested item — the spec'd
    /// response shape strict servers (pyright) require; any other
    /// server-initiated request gets a bare null.
    #[test]
    fn empty_result_shapes_match_spec() {
        let params = serde_json::json!({"items": [{"section": "a"}, {"section": "b"}]});
        assert_eq!(
            empty_result_for("workspace/configuration", Some(&params)),
            serde_json::json!([null, null])
        );
        assert_eq!(
            empty_result_for("workspace/configuration", None),
            serde_json::json!([])
        );
        assert_eq!(
            empty_result_for("client/registerCapability", Some(&params)),
            serde_json::Value::Null
        );
    }
}
