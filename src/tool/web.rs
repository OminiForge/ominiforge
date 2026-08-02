//! The network-egress policy behind the `web_fetch` tool, and the
//! policy-enforcing HTTP client.
//!
//! The threat model is SSRF: the URL being fetched may come from an
//! instruction-injected page or a careless user paste, so the fetch path —
//! not the model, and not the permission gate alone — must guarantee that
//! `web_fetch` can never reach a host the policy forbids. Two attack shapes
//! drive the design:
//!
//! - **Direct**: `http://169.254.169.254/...` — the cloud instance-metadata
//!   endpoint (IMDS), which hands out the VM's IAM credentials to any
//!   unauthenticated GET. Link-local has no legitimate service, so it is a
//!   hardcoded block (`DEFAULT_BLOCKED_CIDRS`).
//! - **DNS rebinding**: `evil.com` resolves publicly when the attacker checks
//!   it, then to a blocked/private address when the agent connects. Defended
//!   by resolving once and pinning every validated address on the client
//!   ([`policy_client`]), so the connection can only go to an IP the policy
//!   already accepted.
//!
//! The other layer of the threat model — "the model was talked into fetching
//! something sensitive" — is deliberately NOT handled here: that is a
//! judgment call, so it belongs to the permission system (ask rules on the
//! `url` field). This module only enforces mechanical invariants: scheme,
//! port, blocked CIDRs, and optional domain allow-lists.
//!
//! Configuration comes from the profile's `[tools.web_fetch]` section
//! ([`crate::config::WebFetchSection`]).

use std::net::IpAddr;
use std::sync::Arc;

use ipnet::IpNet;

/// The hardcoded block floor: link-local (`169.254.0.0/16` and `fe80::/10`),
/// whose only well-known tenant is the cloud metadata service at
/// `169.254.169.254`. Always appended to whatever the config lists, so a
/// permissive config cannot accidentally reopen it.
const HARD_BLOCKED_CIDRS: &[&str] = &["169.254.0.0/16", "fe80::/10"];

/// Networks blocked on top of the hard floor unless the config explicitly
/// narrows `blocked_cidrs`: loopback, RFC-1918 private, CGNAT, unspecified.
///
/// CGNAT (`100.64.0.0/10`) is here rather than in the hard floor because
/// Tailscale numbers its overlay from it — blocking it unconditionally would
/// cut off tailnet-internal debugging, a legitimate use. A deployment that
/// does not use Tailscale keeps the safer default; one that does narrows the
/// list in config.
const DEFAULT_BLOCKED_CIDRS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "::/128",
    "::1/128",
    "fc00::/7",
];

/// The mechanical egress policy for `web_fetch`.
#[derive(Debug, Clone)]
pub struct WebFetchPolicy {
    /// Whether plain `http://` URLs are accepted (`https://` always is).
    pub allow_http: bool,
    /// Ports a URL may target; empty = the scheme default only (80/443).
    pub allowed_ports: Vec<u16>,
    /// Resolved IPs are rejected when inside any of these networks. The hard
    /// link-local floor is always included on top of this list.
    pub blocked_cidrs: Vec<IpNet>,
    /// When non-empty, only URLs whose host matches one of these domain
    /// patterns are fetched (`*.example.com` matches the apex and every
    /// subdomain). An allow-list so strict the tool would be useless without
    /// it is a deliberate operator choice — usually paired with ask rules.
    pub allowed_domains: Vec<String>,
    /// When non-empty, any URL whose host matches one of these patterns is
    /// rejected, regardless of `allowed_domains`.
    pub blocked_domains: Vec<String>,
}

impl Default for WebFetchPolicy {
    fn default() -> Self {
        // Same floor as `from_config` (defaults + hard link-local): the two
        // constructors must agree, or an absent config section would gate
        // differently than an explicit empty one.
        let mut blocked_cidrs = parse_cidrs(DEFAULT_BLOCKED_CIDRS.iter().copied());
        blocked_cidrs.extend(parse_cidrs(HARD_BLOCKED_CIDRS.iter().copied()));
        Self {
            allow_http: true,
            allowed_ports: Vec::new(),
            blocked_cidrs,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
        }
    }
}

impl WebFetchPolicy {
    /// Build a policy from the profile's `[tools.web_fetch]` section.
    ///
    /// `blocked_cidrs` REPLACES the default list when set (so a Tailscale
    /// user can unblock CGNAT); the hard link-local floor is always added.
    /// Unparseable CIDRs fail loud rather than being silently dropped — a
    /// typo in a security list must surface, not weaken the list.
    #[must_use]
    pub fn from_config(section: &crate::config::WebFetchSection) -> Self {
        let mut blocked_cidrs = section.blocked_cidrs.as_ref().map_or_else(
            || parse_cidrs(DEFAULT_BLOCKED_CIDRS.iter().copied()),
            |list| parse_cidrs(list.iter().map(String::as_str)),
        );
        blocked_cidrs.extend(parse_cidrs(HARD_BLOCKED_CIDRS.iter().copied()));
        Self {
            allow_http: section.allow_http,
            allowed_ports: section.allowed_ports.clone(),
            blocked_cidrs,
            allowed_domains: section.allowed_domains.clone(),
            blocked_domains: section.blocked_domains.clone(),
        }
    }

    /// Validate a candidate URL, returning the reason it is refused.
    ///
    /// Checked per redirect hop as well as the original URL: a public URL
    /// that 302s to the metadata service must be refused at the hop.
    ///
    /// # Errors
    /// Returns the refusal reason when the scheme, credentials, host, or
    /// port violates the policy.
    pub fn check_url(&self, url: &url::Url) -> Result<(), String> {
        match url.scheme() {
            "https" => {}
            "http" if self.allow_http => {}
            other => {
                return Err(format!(
                    "scheme `{other}` is not allowed (https only{})",
                    if self.allow_http {
                        "; http is enabled"
                    } else {
                        ""
                    }
                ));
            }
        }
        if url.username() != "" || url.password().is_some() {
            return Err("URLs with embedded credentials are refused".to_owned());
        }
        let host = url.host_str().ok_or_else(|| "URL has no host".to_owned())?;
        if self.blocked_domains.iter().any(|p| domain_matches(p, host)) {
            return Err(format!("host `{host}` is blocked by policy"));
        }
        if !self.allowed_domains.is_empty()
            && !self.allowed_domains.iter().any(|p| domain_matches(p, host))
        {
            return Err(format!("host `{host}` is not in the allowed domains"));
        }
        if let Some(port) = url.port()
            && !self.allowed_ports.contains(&port)
        {
            return Err(format!("port {port} is not in the allowed ports"));
        }
        Ok(())
    }

    /// Whether a resolved address is acceptable to connect to.
    ///
    /// # Errors
    /// Returns the refusal reason when `ip` sits in a blocked CIDR.
    pub fn check_ip(&self, ip: &IpAddr) -> Result<(), String> {
        if self.blocked_cidrs.iter().any(|net| net.contains(ip)) {
            return Err(format!("{ip} is in a blocked network"));
        }
        Ok(())
    }
}

/// Parse CIDR strings, panicking on a malformed entry.
///
/// A panic (not a silent skip) is deliberate: these lists are security
/// floors written by the operator or hardcoded above; a malformed entry at
/// startup must kill the process loudly, never quietly shorten the floor.
fn parse_cidrs<'a>(cidrs: impl IntoIterator<Item = &'a str>) -> Vec<IpNet> {
    cidrs
        .into_iter()
        .map(|c| {
            c.parse()
                .unwrap_or_else(|e| panic!("invalid CIDR `{c}`: {e}"))
        })
        .collect()
}

/// Whether `host` matches a domain pattern.
///
/// - `"*"` matches any host.
/// - `"*.example.com"` matches `example.com` itself AND every subdomain —
///   ergonomics over the strict CSP/TLS reading (where `*.x` excludes the
///   apex): a user writing "allow github's domains" means the apex too.
/// - Any other pattern is an exact match.
#[must_use]
pub fn domain_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host == suffix || host.ends_with(&format!(".{suffix}"));
    }
    host == pattern
}

/// What one successful fetch brought back.
pub struct FetchedPage {
    /// The final URL after any redirects.
    pub final_url: url::Url,
    /// The decoded body text.
    pub body: String,
    /// The response's `Content-Type` header value, if present (e.g.
    /// `text/html; charset=utf-8`). The extraction layer uses this to decide
    /// whether the body is HTML worth running through Readability.
    pub content_type: Option<String>,
}

/// Fetch one URL, enforcing `policy` at every layer.
///
/// Redirects are followed manually (the client is built with
/// `redirect::Policy::none()`) so each hop is re-validated by
/// [`WebFetchPolicy::check_url`] — reqwest's automatic following would chase
/// a 302 into a blocked address without asking. At most `max_redirects` hops
/// are followed.
///
/// The body is decoded per the response's `charset` (reqwest's `charset`
/// feature, honoring the `Content-Type` charset) and must be UTF-8-ish text;
/// a binary response is a business error, not a decode of garbage.
///
/// # Errors
///
/// Returns the reason any hop is refused (policy), the redirect chain breaks
/// (too many hops, missing/invalid `Location`), or the transfer fails.
pub async fn fetch_text(
    policy: &WebFetchPolicy,
    url: &url::Url,
    timeout: std::time::Duration,
    max_redirects: u32,
) -> Result<FetchedPage, String> {
    let mut current = url.clone();
    for hop in 0..=max_redirects {
        let response = fetch_once(policy, &current, timeout).await?;
        if !response.status().is_redirection() {
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let bytes = response
                .bytes()
                .await
                .map_err(|e| format!("failed to read response body: {e}"))?;
            let (text, _, had_errors) = encoding_rs::UTF_8.decode(&bytes);
            if had_errors {
                return Err("response body is not valid UTF-8 (binary content?)".to_owned());
            }
            return Ok(FetchedPage {
                final_url: current,
                body: text.into_owned(),
                content_type,
            });
        }
        if hop == max_redirects {
            return Err(format!("too many redirects (>{max_redirects})"));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                format!(
                    "redirect {} from {current} has no Location header",
                    response.status()
                )
            })?;
        let next = current
            .join(location)
            .map_err(|e| format!("invalid redirect target `{location}`: {e}"))?;
        policy.check_url(&next)?;
        current = next;
    }
    unreachable!("the loop returns or errors by the final hop")
}

/// Execute a single GET against `url`, with the DNS answers pinned.
///
/// The host is resolved once via `lookup_host`; every resolved address is
/// policy-checked, then ALL of them are pinned on the client via
/// `resolve_to_addrs`, so reqwest's own connector can only ever dial an
/// address this function already accepted. Pinning every answer (not one)
/// keeps multi-A/AAAA failover working.
async fn fetch_once(
    policy: &WebFetchPolicy,
    url: &url::Url,
    timeout: std::time::Duration,
) -> Result<reqwest::Response, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_owned())?
        .to_owned();
    let port = url.port_or_known_default().unwrap_or(443);

    // Literal-IP URLs need no lookup; the address is policy-checked directly.
    let addrs: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| format!("failed to resolve {host}: {e}"))?
            .map(|sa| sa.ip())
            .collect(),
    };
    if addrs.is_empty() {
        return Err(format!("{host} resolved to no addresses"));
    }
    for ip in &addrs {
        policy
            .check_ip(ip)
            .map_err(|why| format!("{host} ({ip}): {why}"))?;
    }

    let pinned: Vec<std::net::SocketAddr> = addrs
        .iter()
        .map(|ip| std::net::SocketAddr::new(*ip, port))
        .collect();
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "ominiforge/",
            env!("CARGO_PKG_VERSION"),
            " web_fetch"
        ))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .resolve_to_addrs(&host, &pinned)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))
}

/// A fetcher that caches decoded page contents on disk.
///
/// Every fetched page is written whole into `<workspace>/.ominiforge/cache/`
/// and the model is handed the head plus the cache path, so a truncated
/// answer never strands content: the model pages the rest with the `read`
/// tool (whose workspace jail this directory deliberately lives inside).
#[derive(Debug, Clone)]
pub struct WebCache {
    dir: std::path::PathBuf,
    max_bytes: u64,
}

impl WebCache {
    /// A cache in `<workspace>/.ominiforge/cache` with a `max_bytes` total cap.
    #[must_use]
    pub fn new(workspace: &std::path::Path, max_bytes: u64) -> Self {
        Self {
            dir: workspace.join(".ominiforge").join("cache"),
            max_bytes,
        }
    }

    /// The directory files are stored in (for tests and error messages).
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Store `content` fetched from `url`, returning the workspace-relative
    /// cache path. Enforces the total-size cap first (oldest file evicted
    /// first); a best-effort step — eviction failures are ignored (a full
    /// disk fails the write loudly on its own).
    ///
    /// # Errors
    /// Returns the reason the cache directory or file could not be written.
    pub async fn store(&self, url: &url::Url, content: &str) -> Result<String, String> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| format!("failed to create cache dir: {e}"))?;
        self.enforce_cap(content.len() as u64).await;

        let name = cache_file_name(url);
        tokio::fs::write(self.dir.join(&name), content)
            .await
            .map_err(|e| format!("failed to write cache file: {e}"))?;

        let rel = std::path::Path::new(".ominiforge")
            .join("cache")
            .join(&name);
        Ok(rel.to_string_lossy().into_owned())
    }

    /// Evict oldest-first until `incoming` more bytes fit under the cap.
    async fn enforce_cap(&self, incoming: u64) {
        let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf, u64)> = Vec::new();
        let mut total: u64 = 0;
        let Ok(mut rd) = tokio::fs::read_dir(&self.dir).await else {
            return;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            if let Ok(meta) = entry.metadata().await
                && meta.is_file()
            {
                let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                total += meta.len();
                entries.push((modified, entry.path(), meta.len()));
            }
        }
        entries.sort_by_key(|(modified, _, _)| *modified);
        for (_, path, len) in entries {
            if total + incoming <= self.max_bytes {
                break;
            }
            if tokio::fs::remove_file(&path).await.is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// A stable, filesystem-safe cache file name for a URL: host, a readable
/// slug of the path, and a short content hash of the whole URL so distinct
/// query strings never collide.
fn cache_file_name(url: &url::Url) -> String {
    use sha2::Digest;
    let host = url.host_str().unwrap_or("unknown");
    let slug: String = url
        .path()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    let hash = sha2::Sha256::digest(url.as_str().as_bytes());
    let short = hash[..4].iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    let slug = if slug.is_empty() {
        "index".to_owned()
    } else {
        slug.chars().take(48).collect()
    };
    format!("{host}-{slug}-{short}.md")
}

/// The registry-shared HTTP plumbing is stateless; group the fetches behind
/// a handle so `WebFetchTool` can share one per tool instance if needed.
pub type SharedPolicy = Arc<WebFetchPolicy>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// `*.x.com` covers the apex and every subdomain but NOT a lookalike
    /// (`notx.com`) — the lookalike case is the SSRF-flavored hole in a naive
    /// `ends_with` implementation, so it is the assertion that matters.
    #[test]
    fn wildcard_domain_covers_apex_and_subdomains_only() {
        assert!(domain_matches("*.github.com", "github.com"));
        assert!(domain_matches("*.github.com", "api.github.com"));
        assert!(domain_matches("*.github.com", "a.b.github.com"));
        assert!(!domain_matches("*.github.com", "notgithub.com"));
        assert!(!domain_matches("*.github.com", "github.com.evil.com"));
        assert!(domain_matches("github.com", "github.com"));
        assert!(!domain_matches("github.com", "api.github.com"));
        assert!(domain_matches("*", "anything.example"));
    }

    /// The default policy refuses the metadata service and private/loopback
    /// ranges — the direct-SSRF shapes the floor exists to stop.
    #[test]
    fn default_policy_blocks_metadata_and_private_ips() {
        let policy = WebFetchPolicy::default();
        let blocked = [
            "169.254.169.254",
            "169.254.1.1",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.1.1",
            "0.0.0.0",
            "::1",
        ];
        for ip in blocked {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(policy.check_ip(&addr).is_err(), "{ip} must be blocked");
        }
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        assert!(policy.check_ip(&public).is_ok());
    }

    /// `file:`/`ftp:`/`gopher:` are refused: a network tool must not become a
    /// local-file reader that sidesteps the `read` tool's workspace jail.
    #[test]
    fn non_http_schemes_are_refused() {
        let policy = WebFetchPolicy::default();
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com/",
        ] {
            let url = url::Url::parse(raw).unwrap();
            assert!(policy.check_url(&url).is_err(), "{raw} must be refused");
        }
    }

    /// Embedded credentials (`https://user:pass@host/`) are refused — they
    /// leak into logs/cache names and are never what a fetch needs.
    #[test]
    fn urls_with_credentials_are_refused() {
        let policy = WebFetchPolicy::default();
        let url = url::Url::parse("https://user:pw@example.com/").unwrap();
        assert!(policy.check_url(&url).is_err());
    }

    /// Configured CIDRs REPLACE the default list (minus the hard link-local
    /// floor), so a Tailscale user can unblock CGNAT without reopening the
    /// metadata service.
    #[test]
    fn config_blocked_cidrs_replace_defaults_but_keep_hard_floor() {
        let section = crate::config::WebFetchSection {
            blocked_cidrs: Some(vec!["10.0.0.0/8".to_owned()]),
            ..crate::config::WebFetchSection::default()
        };
        let policy = WebFetchPolicy::from_config(&section);
        let cgnat: IpAddr = "100.64.1.1".parse().unwrap();
        assert!(
            policy.check_ip(&cgnat).is_ok(),
            "narrowed list must unblock CGNAT"
        );
        let metadata: IpAddr = "169.254.169.254".parse().unwrap();
        assert!(
            policy.check_ip(&metadata).is_err(),
            "link-local floor is not configurable away"
        );
    }

    /// An allow-list refuses unlisted hosts; `blocked_domains` wins over
    /// `allowed_domains` (deny-first, mirroring the permission precedence).
    #[test]
    fn domain_allow_and_block_lists() {
        let section = crate::config::WebFetchSection {
            allowed_domains: vec!["*.github.com".to_owned()],
            blocked_domains: vec!["gist.github.com".to_owned()],
            ..crate::config::WebFetchSection::default()
        };
        let policy = WebFetchPolicy::from_config(&section);
        let url = |u: &str| url::Url::parse(u).unwrap();
        assert!(policy.check_url(&url("https://github.com/a")).is_ok());
        assert!(policy.check_url(&url("https://api.github.com/a")).is_ok());
        assert!(policy.check_url(&url("https://example.com/a")).is_err());
        assert!(policy.check_url(&url("https://gist.github.com/a")).is_err());
    }

    /// Cache naming is stable per URL, differs by query string, and stays
    /// within a single safe file name (no separators).
    #[test]
    fn cache_names_are_stable_unique_and_safe() {
        let a = url::Url::parse("https://example.com/docs/rust?p=1").unwrap();
        let b = url::Url::parse("https://example.com/docs/rust?p=2").unwrap();
        let name_a = cache_file_name(&a);
        assert_eq!(name_a, cache_file_name(&a));
        assert_ne!(name_a, cache_file_name(&b));
        assert!(!name_a.contains('/') && !name_a.contains('\\'));
        assert!(name_a.starts_with("example.com-"));
    }

    /// The cap evicts oldest-first so a cache cannot grow without bound.
    #[tokio::test]
    async fn cache_evicts_oldest_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WebCache::new(dir.path(), 100);
        let mk = |s: &str| url::Url::parse(s).unwrap();
        let p1 = cache
            .store(&mk("https://a.com/1"), &"x".repeat(60))
            .await
            .unwrap();
        // Ensure a distinct (later) mtime for the second file.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let p2 = cache
            .store(&mk("https://a.com/2"), &"y".repeat(60))
            .await
            .unwrap();
        assert!(
            !dir.path().join(&p1).exists(),
            "oldest file must be evicted to fit the cap"
        );
        assert!(dir.path().join(&p2).exists());
    }
}
