//! The approval gate: how the agent resolves a [`Decision::Ask`] into a
//! run-or-block verdict for a single tool call.
//!
//! The [`permission`](crate::permission) policy decides *whether* a call needs
//! human sign-off; this trait decides *how* that human is reached. It is the
//! same shape of dependency as [`StreamSink`](super::StreamSink): the agent core
//! stays front-end-agnostic, and each front-end supplies its own gate — the
//! gateway routes a request/response to the client over the wire
//! (`doc/permission.md` §5). The agent never blocks the turn itself; it
//! awaits [`ApprovalGate::request`], and the gate implementation owns the
//! suspend/resume mechanics.
//!
//! The default, [`NullGate`], is **fail-closed**: with no real gate wired
//! (headless scheduler, eval, tests), an `Ask` is rejected rather than silently
//! allowed. Safety must not depend on a gate being present (`CLAUDE.md` §12).

use std::sync::Arc;

/// A pending tool call awaiting a human decision. Handed to
/// [`ApprovalGate::request`] when the policy returns [`Decision::Ask`].
///
/// [`Decision::Ask`]: crate::permission::Decision::Ask
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// The tool the model wants to run (e.g. `"shell"`).
    pub tool_name: String,
    /// The decoded, post-hook input the tool would receive — what the human is
    /// approving. The gate renders this for the user to inspect.
    pub input: serde_json::Value,
    /// The model-assigned call id, so a gate that routes decisions
    /// asynchronously (the gateway) can correlate a reply to the right call.
    pub call_id: String,
}

/// A human's answer to an [`ApprovalRequest`]. Serialized `snake_case`
/// (`approve` / `reject`) so a gateway client can send it on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Run the tool.
    Approve,
    /// Block the tool; the agent feeds a `denied_by_user` error back to the model.
    Reject,
}

/// How far a human's decision reaches beyond the single call that prompted it
/// (`doc/permission.md` §5). Serialized `snake_case` so a gateway client can
/// send it on the wire alongside the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// This call only — the resting case; nothing is pinned anywhere.
    Once,
    /// Pin the decision for the rest of this session: compiled to a rule and
    /// written into the session's live in-memory policy.
    Session,
    /// Pin the decision durably for every session on this profile: written into
    /// the profile TOML (and the live session policy).
    Profile,
    /// Pin the decision gateway-wide: written into `gateway.toml` (and the live
    /// session policy).
    Gateway,
}

/// How a gate *resolved* an [`ApprovalRequest`].
///
/// Richer than [`ApprovalDecision`] (the human-facing / wire answer) because the
/// agent must audit *why* a call was blocked: a human's explicit `no` and a
/// fail-closed auto-denial (no gate wired, a dropped channel, a non-interactive
/// terminal) are different facts and must not both be recorded as "the user
/// rejected it" (`doc/permission.md` §6, `CLAUDE.md` §12). A gate returns this;
/// `dispatch_tool` maps it to a
/// [`PermissionOutcome`](crate::core::payload::PermissionOutcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalResolution {
    /// A human approved the call.
    Approved,
    /// A human explicitly rejected the call.
    RejectedByUser,
    /// A rule pinned by an earlier scoped decision resolved the ask — no fresh
    /// human answer was given for this call. `approved` is the rule's verdict
    /// (`allow` pinned → true, `deny` pinned → false). Audited as
    /// `decided_by: "policy"` (`doc/permission.md` §5.1).
    PinnedByRule {
        /// Whether the pinned rule allowed (`true`) or denied (`false`).
        approved: bool,
    },
    /// No human decided: the gate failed closed (no approver reachable). This is
    /// still a block, but nobody was consulted.
    AutoDenied,
}

/// A gate's full answer to an [`ApprovalRequest`].
///
/// The three-way [`ApprovalResolution`] plus, when a human decided, the
/// [`ApprovalScope`] they chose (`None` for an auto-denial — nobody decided, so
/// there is no scope to record).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalOutcome {
    /// What happened to the call (run / blocked / fail-closed).
    pub resolution: ApprovalResolution,
    /// How far the human's decision reaches; `None` when no human decided.
    pub scope: Option<ApprovalScope>,
}

/// Resolves an [`ApprovalRequest`] to an [`ApprovalOutcome`].
///
/// Implementations own the interaction: a terminal gate prompts and reads stdin;
/// the gateway gate publishes an `AwaitingInput` status and awaits a command
/// carrying the decision. `request` may suspend for as long as the human takes —
/// the turn is paused, not spinning.
///
/// `Send + Sync` because the agent is shared (`Arc<Agent>`) across worker
/// threads and the gate is called from the turn task.
#[async_trait::async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Ask the human whether `req` may run, resolving when they answer (or when
    /// the gate gives up and fails closed).
    async fn request(&self, req: ApprovalRequest) -> ApprovalOutcome;

    /// Whether the gate can field multiple concurrent `request`s. A gate that
    /// routes decisions over a shared channel (the gateway) returns `true`, so
    /// the agent dispatches a round's tool calls in two phases — preparing all
    /// of them (and publishing every `ask`) before settling any. Other gates
    /// keep the serial default: the agent then dispatches one call at a time.
    fn supports_concurrent_requests(&self) -> bool {
        false
    }
}

/// The fail-closed default gate: every request resolves to
/// [`AutoDenied`](ApprovalResolution::AutoDenied).
///
/// Used wherever no interactive front-end is present (headless runs, eval,
/// tests). It guarantees an `Ask` never becomes an implicit `Allow` just because
/// nobody wired a real gate.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullGate;

#[async_trait::async_trait]
impl ApprovalGate for NullGate {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
        // No human is present, so this is an auto-denial, not a user rejection.
        ApprovalOutcome {
            resolution: ApprovalResolution::AutoDenied,
            scope: None,
        }
    }
}

/// The default gate handle the agent starts with: a shared [`NullGate`].
#[must_use]
pub fn default_gate() -> Arc<dyn ApprovalGate> {
    Arc::new(NullGate)
}
