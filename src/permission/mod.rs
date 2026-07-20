//! The permission subsystem: a declarative, code-enforced gate that classifies
//! every tool call as `allow` / `deny` / `ask` *before* it runs.
//!
//! This is the "safety cannot rely on trusting the model, it must rely on code"
//! principle (`doc/permission.md`): the model proposes a tool call, but a
//! [`PermissionPolicy`] — not the model — decides whether it may proceed. The
//! agent loop consults [`PermissionPolicy::evaluate`] in `dispatch_tool` after
//! the `tool:invoke:before` hooks have run (so it judges the final, possibly
//! hook-rewritten input) and before the tool executes.
//!
//! The policy is a two-list rule table evaluated in fixed precedence
//! (`evaluate`): a matching **deny** rule wins over a matching **ask** rule, and
//! a call matching neither is **allowed**. This module is pure decision logic —
//! it performs no I/O and knows nothing about how `Ask` is resolved (that is the
//! agent's `ApprovalGate`) or how the table is configured (that is
//! `[permission]` in `doc/profile.md`).

use serde::{Deserialize, Serialize};

/// What the policy decided for one tool call.
///
/// Mirrors the reference three-behavior model: `Allow` runs the tool directly,
/// `Deny` blocks it (fed back to the model as a tool error), and `Ask` suspends
/// the call for a human decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run the tool without interruption. The resting case for everyday calls.
    Allow,
    /// Block the tool. The agent surfaces this to the model as a `denied_by_policy`
    /// tool error, never executing the tool.
    Deny,
    /// Pause and ask a human. The agent routes this through its approval gate;
    /// how the human is prompted depends on the front-end.
    Ask,
}

/// How a [`Rule`]'s patterns are compared against the input text.
///
/// The set is deliberately small — the config UI compiles its per-tool controls
/// (e.g. a directory allow-list) down to these primitives, so every mode must be
/// something a non-expert can reason about. Richer matching (glob / regex) can
/// be added as new variants without breaking existing rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// A pattern matches when it occurs anywhere as a substring. The default, and
    /// what the legacy `contains` list always meant.
    #[default]
    Substring,
    /// A pattern matches when the text starts with it — path prefixes
    /// (`/etc/`, `src/`). Backs the per-tool path allow/deny-list controls.
    Prefix,
}

/// One rule in a [`PermissionPolicy`]: it matches a tool call when the call's
/// tool name matches [`tool`](Self::tool) **and** the input satisfies the
/// rule's field / mode / pattern test.
///
/// A rule carries no verdict of its own — which list (`deny` or `ask`) it sits
/// in is the verdict (`doc/permission.md` §3). This keeps the table readable:
/// every rule under `deny` denies, every rule under `ask` asks.
///
/// The structured shape (`field` + `mode` + `negate`) is what the config UI's
/// per-tool cards compile to; a hand-written TOML rule that sets only `contains`
/// still works (see the field docs).
///
/// [`Default`] yields an empty rule with `tool = ""`, which matches no real tool
/// — a harmless base for `..Default::default()` in construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct Rule {
    /// The tool name this rule applies to. `"*"` matches any tool; otherwise the
    /// match is exact. A rule scoped to `"*"` with no patterns matches every call
    /// (a catch-all).
    pub tool: String,

    /// Which input field the patterns test. `None` searches **every** string
    /// value in the input recursively (the legacy behavior — a pattern matches
    /// wherever it lands). `Some("command")` restricts the test to that
    /// top-level field, so a `shell.command` rule never accidentally fires on
    /// some other string. A named field that is absent/non-string yields no text
    /// (the rule then matches only if [`negate`](Self::negate) is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,

    /// How [`patterns`](Self::patterns) are compared (substring / prefix).
    #[serde(default, skip_serializing_if = "MatchMode::is_default")]
    pub mode: MatchMode,

    /// The patterns to test. Aliased to `contains` on disk for backward
    /// compatibility: an existing `contains = [...]` rule deserializes straight
    /// into this field, and it serializes back as `contains` so old files keep
    /// their shape.
    ///
    /// An empty list means "match any input for this tool" (a tool-level rule,
    /// e.g. deny the `shell` tool outright) — subject to `negate`.
    #[serde(
        default,
        alias = "contains",
        rename = "contains",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub patterns: Vec<String>,

    /// Invert the pattern test. `false` (default) = match when a pattern hits;
    /// `true` = match when **no** pattern hits. This is how an allow-list is
    /// expressed: "ask `write` when `path` does **not** start with any of
    /// `src/`, `tmp/`" is `negate = true`, `mode = prefix`, `field = "path"`. A
    /// `negate` rule with an empty pattern list never matches (nothing to fail),
    /// so an empty allow-list does not lock the tool out by accident.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub negate: bool,
}

impl MatchMode {
    /// Whether this is the default mode — used by `skip_serializing_if` so a
    /// plain substring rule serializes without a redundant `mode = "substring"`.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Substring)
    }

    /// Whether `text` matches `pattern` under this mode.
    #[must_use]
    fn hit(self, text: &str, pattern: &str) -> bool {
        match self {
            Self::Substring => text.contains(pattern),
            Self::Prefix => text.starts_with(pattern),
        }
    }
}

impl Rule {
    /// A substring rule over the whole input (the legacy shape): the common
    /// constructor for simple denials and tests. Pass an empty `patterns` for a
    /// tool-level rule (matches any input for `tool`).
    #[must_use]
    pub fn contains(tool: impl Into<String>, patterns: Vec<String>) -> Self {
        Self {
            tool: tool.into(),
            patterns,
            ..Self::default()
        }
    }

    /// Whether this rule matches a call to `tool` with the given decoded `input`.
    #[must_use]
    pub fn matches(&self, tool: &str, input: &serde_json::Value) -> bool {
        if self.tool != "*" && self.tool != tool {
            return false;
        }
        // An empty, non-negated pattern list is a tool-level rule: it matches any
        // input for this tool. (A negated empty list has nothing to fail, so it
        // never matches — guarded here so an empty allow-list is not a lock-out.)
        if self.patterns.is_empty() {
            return !self.negate;
        }
        let hit = self.pattern_hits(input);
        // `negate` inverts: the rule matches when NO pattern hit.
        hit != self.negate
    }

    /// Whether any pattern hits, honoring the rule's field scope and mode.
    fn pattern_hits(&self, input: &serde_json::Value) -> bool {
        // The text to search: a field-scoped rule looks at just that top-level
        // field (absent → no text → no hit); an unscoped rule searches the whole
        // input recursively (legacy behavior).
        let hit_in = |value: &serde_json::Value| {
            self.patterns
                .iter()
                .any(|pattern| value_hits(value, pattern, self.mode))
        };
        self.field.as_ref().map_or_else(
            || hit_in(input),
            |name| input.get(name).is_some_and(hit_in),
        )
    }
}

/// A tool-call gate: two ordered rule lists whose precedence is fixed by
/// [`evaluate`](Self::evaluate).
///
/// The empty policy allows everything, so a profile with no `[permission]`
/// section imposes no gate — existing behavior is preserved
/// (`doc/permission.md` §2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct PermissionPolicy {
    /// Rules that, when matched, block the call. Highest precedence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<Rule>,
    /// Rules that, when matched (and no deny rule matched), require human
    /// approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<Rule>,
}

impl PermissionPolicy {
    /// Classify a tool call. Precedence is fixed and mirrors the reference
    /// three-gate pipeline: a matching `deny` rule wins outright; otherwise a
    /// matching `ask` rule requires approval; otherwise the call is allowed.
    #[must_use]
    pub fn evaluate(&self, tool: &str, input: &serde_json::Value) -> Decision {
        if self.deny.iter().any(|r| r.matches(tool, input)) {
            return Decision::Deny;
        }
        if self.ask.iter().any(|r| r.matches(tool, input)) {
            return Decision::Ask;
        }
        Decision::Allow
    }

    /// Whether the policy has no rules at all — the agent skips evaluation
    /// entirely in that case, preserving the pre-permission fast path.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.deny.is_empty() && self.ask.is_empty()
    }

    /// Layer this policy (the higher-precedence override) over `base` (the
    /// lower-precedence default), producing the effective policy.
    ///
    /// The two lists merge with **different** rules, matching profile overlay
    /// (`doc/permission.md` §3) and the three-tier resolution
    /// (`workspace > profile > gateway`, `resolve_permission` in `app.rs`):
    ///
    /// - `deny` is **union-inherited** — a security floor. The override may add
    ///   denials but can never drop one it inherited (dropping would be a stealth
    ///   privilege escalation). Duplicates are collapsed so repeated layering is
    ///   idempotent.
    /// - `ask` is **replace-or-inherit** — a non-empty override `ask` list wins
    ///   wholesale; an empty one inherits `base`'s. `ask` is only a confirmation
    ///   prompt, not a floor, so an intentional layer may replace it.
    #[must_use]
    pub fn layer_over(self, base: Self) -> Self {
        let mut deny = base.deny;
        for rule in self.deny {
            if !deny.contains(&rule) {
                deny.push(rule);
            }
        }
        let ask = if self.ask.is_empty() { base.ask } else { self.ask };
        Self { deny, ask }
    }
}

/// Whether `pattern` matches (under `mode`) any string value reachable in
/// `value` (recursing into arrays and objects; object *keys* are not searched,
/// only values). This is the matching primitive behind [`Rule::patterns`]: a
/// field-scoped rule passes that field's value here, an unscoped rule passes the
/// whole input.
fn value_hits(value: &serde_json::Value, pattern: &str, mode: MatchMode) -> bool {
    match value {
        serde_json::Value::String(s) => mode.hit(s, pattern),
        serde_json::Value::Array(items) => items.iter().any(|v| value_hits(v, pattern, mode)),
        serde_json::Value::Object(map) => map.values().any(|v| value_hits(v, pattern, mode)),
        // Numbers, booleans, and null carry no text to match against.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    fn rule(tool: &str, contains: &[&str]) -> Rule {
        Rule::contains(tool, contains.iter().map(|s| (*s).to_owned()).collect())
    }

    /// The empty policy is a no-op: every call is allowed, so an unconfigured
    /// profile behaves exactly as before permissions existed.
    #[test]
    fn empty_policy_allows_everything() {
        let policy = PermissionPolicy::default();
        assert!(policy.is_empty());
        assert_eq!(
            policy.evaluate("shell", &json!({"command": "rm -rf /"})),
            Decision::Allow
        );
    }

    /// A `deny` rule matching a substring of the input blocks the call — this is
    /// the hard block the whole subsystem exists to guarantee. A test that only
    /// checked a benign command would not fail if the deny path silently broke.
    #[test]
    fn deny_rule_blocks_matching_command() {
        let policy = PermissionPolicy {
            deny: vec![rule("shell", &["rm -rf"])],
            ask: vec![],
        };
        assert_eq!(
            policy.evaluate("shell", &json!({"command": "rm -rf /etc"})),
            Decision::Deny
        );
        // A shell call without the pattern is untouched by the rule.
        assert_eq!(
            policy.evaluate("shell", &json!({"command": "ls -la"})),
            Decision::Allow
        );
    }

    /// Deny outranks ask: when a call matches both an ask rule and a deny rule,
    /// it must be denied. This encodes the security-critical precedence — if it
    /// inverted, a "please confirm" rule could downgrade an outright ban.
    #[test]
    fn deny_wins_over_ask() {
        let policy = PermissionPolicy {
            deny: vec![rule("shell", &["rm -rf"])],
            ask: vec![rule("shell", &[])], // ask on any shell call
        };
        assert_eq!(
            policy.evaluate("shell", &json!({"command": "rm -rf /"})),
            Decision::Deny
        );
        // A shell call that misses the deny pattern still falls through to ask.
        assert_eq!(
            policy.evaluate("shell", &json!({"command": "echo hi"})),
            Decision::Ask
        );
    }

    /// An `ask` rule with no `contains` gates a whole tool, and the wildcard
    /// tool `"*"` gates every tool — the two coarse-grained knobs a config
    /// author reaches for first.
    #[test]
    fn tool_level_and_wildcard_rules() {
        let tool_level = PermissionPolicy {
            deny: vec![],
            ask: vec![rule("write", &[])],
        };
        assert_eq!(tool_level.evaluate("write", &json!({"path": "a"})), Decision::Ask);
        assert_eq!(tool_level.evaluate("read", &json!({"path": "a"})), Decision::Allow);

        let wildcard = PermissionPolicy {
            deny: vec![rule("*", &["/etc/"])],
            ask: vec![],
        };
        assert_eq!(
            wildcard.evaluate("write", &json!({"path": "/etc/passwd"})),
            Decision::Deny
        );
        assert_eq!(
            wildcard.evaluate("read", &json!({"path": "/etc/shadow"})),
            Decision::Deny
        );
    }

    /// Patterns match string values found anywhere in the input, including
    /// nested arrays/objects — so a rule need not know the exact field the model
    /// happened to use.
    #[test]
    fn matching_recurses_into_nested_values() {
        let policy = PermissionPolicy {
            deny: vec![rule("shell", &["sudo"])],
            ask: vec![],
        };
        assert_eq!(
            policy.evaluate("shell", &json!({"argv": ["sudo", "reboot"]})),
            Decision::Deny
        );
        // Keys are not searched — a field literally named "sudo" is not a match.
        assert_eq!(
            policy.evaluate("shell", &json!({"sudo": false, "command": "ls"})),
            Decision::Allow
        );
    }

    /// A legacy `contains = [...]` TOML rule still parses and behaves as before —
    /// the on-disk alias is the backward-compat contract. This must fail if the
    /// alias/rename is dropped, so old config files silently stop gating.
    #[test]
    fn legacy_contains_alias_round_trips() {
        let parsed: Rule = toml::from_str("tool = \"shell\"\ncontains = [\"rm -rf\"]\n").unwrap();
        assert_eq!(parsed, Rule::contains("shell", vec!["rm -rf".to_owned()]));
        assert!(parsed.matches("shell", &json!({"command": "rm -rf /"})));
        // It serializes back to `contains`, not `patterns`, so files keep shape.
        let text = toml::to_string(&parsed).unwrap();
        assert!(text.contains("contains"), "must serialize as contains, got: {text}");
        assert!(!text.contains("patterns"));
    }

    /// A field-scoped rule tests ONLY that field: a pattern that would match some
    /// other string in the input does not fire. This is the precision the config
    /// UI relies on — a `shell.command` rule must not trip on an unrelated field.
    #[test]
    fn field_scoped_rule_ignores_other_fields() {
        let policy = PermissionPolicy {
            deny: vec![Rule {
                tool: "shell".to_owned(),
                field: Some("command".to_owned()),
                patterns: vec!["danger".to_owned()],
                ..Rule::default()
            }],
            ask: vec![],
        };
        // Hit in the scoped field → deny.
        assert_eq!(
            policy.evaluate("shell", &json!({"command": "danger --now"})),
            Decision::Deny
        );
        // Same text in a DIFFERENT field → not matched (unscoped would have).
        assert_eq!(
            policy.evaluate("shell", &json!({"command": "ls", "note": "danger"})),
            Decision::Allow
        );
    }

    /// Prefix mode anchors at the start — a path allow/deny-list primitive. A
    /// substring hit that is not a prefix must NOT match, or a deny-list on
    /// `/etc/` would over-fire on `/home/etc/`.
    #[test]
    fn prefix_mode_anchors_at_start() {
        let policy = PermissionPolicy {
            deny: vec![Rule {
                tool: "read".to_owned(),
                field: Some("path".to_owned()),
                mode: MatchMode::Prefix,
                patterns: vec!["/etc/".to_owned()],
                ..Rule::default()
            }],
            ask: vec![],
        };
        assert_eq!(policy.evaluate("read", &json!({"path": "/etc/passwd"})), Decision::Deny);
        // Contains "/etc/" but not as a prefix -> allowed (substring would deny).
        assert_eq!(policy.evaluate("read", &json!({"path": "home/x/etc/y"})), Decision::Allow);
    }

    /// `negate` expresses an allow-list: ask `write` when `path` does NOT start
    /// with any allowed prefix. The security-critical assertion is that a path
    /// OUTSIDE the list triggers (asks) while one inside is silent — an inverted
    /// negate would ask exactly on the permitted paths.
    #[test]
    fn negate_expresses_allowlist() {
        let policy = PermissionPolicy {
            deny: vec![],
            ask: vec![Rule {
                tool: "write".to_owned(),
                field: Some("path".to_owned()),
                mode: MatchMode::Prefix,
                patterns: vec!["src/".to_owned(), "tmp/".to_owned()],
                negate: true,
            }],
        };
        // Outside the allow-list → ask.
        assert_eq!(policy.evaluate("write", &json!({"path": "etc/shadow"})), Decision::Ask);
        // Inside the allow-list → allowed (silent).
        assert_eq!(policy.evaluate("write", &json!({"path": "src/main.rs"})), Decision::Allow);
    }

    /// An empty `negate` allow-list must NOT lock the tool out: with nothing in
    /// the list there is nothing to fail, so the rule never matches. A naive
    /// `hit != negate` on an empty list would match everything and deny-all.
    #[test]
    fn empty_negate_allowlist_is_inert() {
        let rule = Rule {
            tool: "write".to_owned(),
            field: Some("path".to_owned()),
            mode: MatchMode::Prefix,
            patterns: vec![],
            negate: true,
        };
        assert!(!rule.matches("write", &json!({"path": "anywhere"})));
    }

    /// `layer_over` is the shared merge behind both profile overlay and the
    /// three-tier resolution: `deny` unions (security floor — an override can add
    /// but never drop an inherited ban), `ask` replaces when the override sets
    /// any rule. A test that only checked the union would miss the ask-replace
    /// half; one that only checked ask-replace would miss the escalation guard.
    #[test]
    fn layer_over_unions_deny_and_replaces_ask() {
        let base = PermissionPolicy {
            deny: vec![rule("shell", &["rm -rf"])],
            ask: vec![rule("read", &[])],
        };
        let over = PermissionPolicy {
            deny: vec![rule("net", &[])],
            ask: vec![rule("write", &[])],
        };
        let merged = over.layer_over(base);
        // Base deny survived AND the override's deny was added (union floor).
        assert_eq!(merged.evaluate("shell", &json!({"command": "rm -rf /"})), Decision::Deny);
        assert_eq!(merged.evaluate("net", &json!({})), Decision::Deny);
        // The override's non-empty ask replaced the base's — base's `read` ask is gone.
        assert_eq!(merged.evaluate("read", &json!({"path": "x"})), Decision::Allow);
        assert_eq!(merged.evaluate("write", &json!({"path": "x"})), Decision::Ask);
    }

    /// An empty override contributes nothing: `deny` unchanged, `ask` inherited.
    /// This is the fast path for a tier that sets no `[permission]` section — it
    /// must be a true no-op, not silently wipe a lower tier's ask list.
    #[test]
    fn layer_over_empty_override_inherits_base() {
        let base = PermissionPolicy {
            deny: vec![rule("shell", &["rm -rf"])],
            ask: vec![rule("write", &[])],
        };
        let merged = PermissionPolicy::default().layer_over(base.clone());
        assert_eq!(merged, base);
    }
}
