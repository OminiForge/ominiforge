//! The gateway's approval gate: how a session actor resolves a tool call the
//! permission policy classified as `ask` (`doc/permission.md` §5).
//!
//! The turn runs on a spawned task (`actor::run_turn_phase`) while the actor's
//! command loop keeps listening on its inbox. When the agent hits an `ask`, it
//! awaits [`GatewayApprovalGate::request`], which:
//!   1. parks a [`oneshot::Sender`] in a shared [`PendingApprovals`] table keyed
//!      by the tool call id,
//!   2. publishes `AwaitingApproval` to the status hub (lighting the session up
//!      in the list) and emits an `ApprovalRequested` event on the outbound
//!      stream so a connected client can prompt the user, then
//!   3. suspends on the receiver.
//!
//! A client's decision arrives as a `Command::Approve { call_id, decision,
//! scope }`, which the actor routes by removing the matching sender from the
//! table and sending the decision — waking the parked turn. If the table entry
//! is dropped instead (turn cancelled, actor shutting down), the receiver
//! errors and the gate resolves to [`ApprovalResolution::AutoDenied`]:
//! **fail-closed**, and audited as a no-human denial rather than a user
//! rejection (`CLAUDE.md` §12).
//!
//! Every decision carries an [`ApprovalScope`]. `Once` decides only the pending
//! call. The wider scopes pin the decision: the call is compiled into a
//! [`Rule`] ([`rule_from_call`]) and merged into the session's live policy
//! (approve → `allow`, reject → `deny`), effective on the next tool call, and —
//! for the two durable scopes (`Profile` / `Gateway`) — also handed to the
//! `on_scoped` callback the registry injects to persist it (profile TOML /
//! `gateway.toml`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{broadcast, oneshot};

use crate::agent::{
    ApprovalDecision, ApprovalGate, ApprovalOutcome, ApprovalRequest, ApprovalResolution,
    ApprovalScope,
};
use crate::core::SessionId;
use crate::permission::{Decision, PermissionPolicy, Rule, rule_from_call};

use super::actor::GatewayEvent;
use super::status::{ActivityStatus, SessionStatus, StatusHub};
use super::workspace::WorkspaceId;

/// A decision delivered to a parked ask.
#[derive(Debug, Clone, Copy)]
pub struct PendingAnswer {
    /// `approve` runs the tool; `reject` blocks it.
    pub decision: ApprovalDecision,
    /// How far the decision reaches.
    pub scope: ApprovalScope,
    /// `true` when a rule pinned by an earlier scoped decision resolved this
    /// ask — no fresh human answer was given for this call, so the audit
    /// attributes it to the rule (`decided_by: "policy"`), not to a user.
    pub pinned_by_rule: bool,
}

/// One parked approval waiter: the oneshot the actor resolves, plus the call's
/// tool name and input — kept so a scoped pin can re-evaluate still-pending
/// asks against the updated policy (`doc/permission.md` §5.1).
pub struct PendingEntry {
    /// Where the answer is delivered.
    pub sender: oneshot::Sender<PendingAnswer>,
    /// The parked call's tool name (for policy re-evaluation).
    pub tool_name: String,
    /// The parked call's post-hook input (for policy re-evaluation).
    pub input: serde_json::Value,
}

/// Parked approval waiters, keyed by tool call id. Shared between the gate (which
/// inserts on `ask`) and the actor (which removes-and-sends on `Command::Approve`,
/// and clears on cancel/shutdown). A `std::sync::Mutex` is enough: every critical
/// section is a single map op with no `.await` held across the lock.
pub type PendingApprovals = Arc<Mutex<HashMap<String, PendingEntry>>>;

/// A human decision carrying a durable scope (`profile` / `gateway`), handed to
/// the gate's `on_scoped` callback so the layer that owns the config files (the
/// registry) can persist the compiled rule.
#[derive(Debug, Clone)]
pub struct ScopedDecision {
    /// What the human answered (`approve` pins into `allow`, `reject` into
    /// `deny`).
    pub decision: ApprovalDecision,
    /// Where they asked it pinned — `Profile` or `Gateway`; the ephemeral
    /// scopes never reach the callback.
    pub scope: ApprovalScope,
    /// The rule compiled from the approved/rejected call (`rule_from_call`).
    pub rule: Rule,
}

/// The per-session approval gate handed to the agent at spawn.
pub struct GatewayApprovalGate {
    /// Where waiters park; the actor drains this to deliver decisions.
    pending: PendingApprovals,
    /// The session's outbound stream — carries the `ApprovalRequested` event.
    outbound: broadcast::Sender<GatewayEvent>,
    /// Process-wide status publisher, so the list shows `AwaitingApproval`.
    status: StatusHub,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    /// The session's latest committed `seq`, kept current by the actor's event
    /// forwarder — stamped on each published status (same source the actor uses).
    latest_seq: Arc<AtomicU64>,
    /// The session's live policy, shared with the agent: scoped decisions are
    /// pinned here and take effect on the next `dispatch_tool` evaluation.
    policy: Arc<RwLock<PermissionPolicy>>,
    /// Persists `profile`/`gateway`-scoped decisions. Owned by the registry
    /// (the layer holding the config roots); a persistence failure is logged
    /// there and never fails the approval itself.
    on_scoped: Option<Arc<dyn Fn(ScopedDecision) + Send + Sync>>,
}

impl GatewayApprovalGate {
    /// Build a gate sharing the actor's `pending` table, outbound stream, status
    /// hub, latest-seq cache, and the agent's live policy handle.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        pending: PendingApprovals,
        outbound: broadcast::Sender<GatewayEvent>,
        status: StatusHub,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        latest_seq: Arc<AtomicU64>,
        policy: Arc<RwLock<PermissionPolicy>>,
        on_scoped: Option<Arc<dyn Fn(ScopedDecision) + Send + Sync>>,
    ) -> Self {
        Self {
            pending,
            outbound,
            status,
            session_id,
            workspace_id,
            latest_seq,
            policy,
            on_scoped,
        }
    }

    /// Publish one activity-status transition for this session.
    fn publish(&self, status: ActivityStatus) {
        self.status.publish(SessionStatus {
            session_id: self.session_id.clone(),
            workspace_id: self.workspace_id.clone(),
            status,
            latest_seq: self.latest_seq.load(Ordering::Relaxed),
        });
    }

    /// Pin a scoped decision: compile the call into a rule and merge it into
    /// the session's live policy (approve → `allow`, reject → `deny`,
    /// duplicates skipped), re-evaluate the asks still pending against the
    /// updated policy, then hand `profile`/`gateway` scopes to the persistence
    /// callback.
    fn pin_scoped(&self, req: &ApprovalRequest, decision: ApprovalDecision, scope: ApprovalScope) {
        let rule = rule_from_call(
            &req.tool_name,
            &req.input,
            primary_field_of(&req.tool_name).as_deref(),
        );
        // A poisoned lock still holds an intact policy (a panic cannot tear a
        // `Vec<Rule>` push), so recover the guard rather than drop the pin.
        let mut policy = self
            .policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let list = match decision {
            ApprovalDecision::Approve => &mut policy.allow,
            ApprovalDecision::Reject => &mut policy.deny,
        };
        if !list.contains(&rule) {
            list.push(rule.clone());
        }
        // Release the write guard before re-evaluating and persisting.
        drop(policy);
        // The pin changed the session's policy: every ask still parked may
        // already be decided by the new rules (`doc/permission.md` §5.1).
        self.resolve_pending_by_rule(scope);
        if matches!(scope, ApprovalScope::Profile | ApprovalScope::Gateway)
            && let Some(on_scoped) = &self.on_scoped
        {
            on_scoped(ScopedDecision {
                decision,
                scope,
                rule,
            });
        }
    }

    /// Re-evaluate every parked ask against the just-pinned policy and
    /// auto-resolve the ones a rule now decides: `allow` auto-approves, `deny`
    /// auto-rejects — carrying the pin's scope and marked `pinned_by_rule` so
    /// the audit attributes them to the rule, not to a fresh human answer. A
    /// real human answer racing the pin wins or loses cleanly:
    /// `HashMap::remove` hands each entry to exactly one resolver.
    fn resolve_pending_by_rule(&self, scope: ApprovalScope) {
        // Recover a poisoned guard rather than skip the re-evaluation (same
        // argument as in `pin_scoped`); a poisoned pending lock stays
        // fail-closed — every parked ask auto-denies on `clear_pending`.
        let policy = self
            .policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        // Decide under both guards, deliver after they are released.
        let mut resolved = Vec::new();
        let call_ids: Vec<String> = pending.keys().cloned().collect();
        for call_id in call_ids {
            let auto = match pending.get(&call_id) {
                Some(entry) => match policy.evaluate(&entry.tool_name, &entry.input) {
                    Decision::Allow => ApprovalDecision::Approve,
                    Decision::Deny => ApprovalDecision::Reject,
                    Decision::Ask => continue,
                },
                None => continue,
            };
            // Take the entry out first, so no human answer can deliver a
            // duplicate resolution into it.
            if let Some(entry) = pending.remove(&call_id) {
                resolved.push((
                    entry.sender,
                    PendingAnswer {
                        decision: auto,
                        scope,
                        pinned_by_rule: true,
                    },
                ));
            }
        }
        drop(pending);
        drop(policy);
        for (sender, answer) in resolved {
            let _ = sender.send(answer);
        }
    }
}

/// The tool's primary input field: the first field key of its built-in catalog
/// entry (`command` for `shell`, `path` for `read`/`write`/`edit`). `None` for
/// tools outside the catalog (MCP tools) — their pinned rules degrade to bare
/// tool-level rules.
fn primary_field_of(tool: &str) -> Option<String> {
    crate::tool::builtin_catalog()
        .into_iter()
        .find(|info| info.name == tool)
        .and_then(|info| info.fields.into_iter().next())
        .map(|field| field.key)
}

#[async_trait::async_trait]
impl ApprovalGate for GatewayApprovalGate {
    async fn request(&self, req: ApprovalRequest) -> ApprovalOutcome {
        let (tx, rx) = oneshot::channel();
        // Park the waiter before announcing, so a decision can never race in
        // ahead of the table entry. A poisoned lock drops `tx` here, so `rx`
        // errors below → `AutoDenied` — fail-closed. The entry also carries the
        // call's tool/input so a scoped pin can re-evaluate it (§5.1).
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(
                req.call_id.clone(),
                PendingEntry {
                    sender: tx,
                    tool_name: req.tool_name.clone(),
                    input: req.input.clone(),
                },
            );
        }

        self.publish(ActivityStatus::AwaitingApproval);
        // Ephemeral, like a `Delta`: a client connecting after this fires learns
        // of the pending ask from the `AwaitingApproval` status, not a replay.
        // Cloned — the request itself is still needed after the await to
        // compile a scoped rule.
        let _ = self.outbound.send(GatewayEvent::ApprovalRequested {
            call_id: req.call_id.clone(),
            tool_name: req.tool_name.clone(),
            input: req.input.clone(),
        });

        // Suspend until the actor delivers a decision — or the sender is dropped
        // (cancel/shutdown/all-handles-gone → `clear_pending`). A delivered value
        // is a human decision (or a pinned rule's); a dropped channel is a
        // no-human auto-denial.
        let outcome = rx.await.map_or(
            ApprovalOutcome {
                resolution: ApprovalResolution::AutoDenied,
                scope: None,
            },
            |answer| {
                // A scope wider than `once` pins the decision as a rule. An
                // auto-resolution (`pinned_by_rule`) carries the pin's scope
                // for the audit but must NOT re-pin — the rule already exists
                // (it is what resolved this ask).
                if answer.scope != ApprovalScope::Once && !answer.pinned_by_rule {
                    self.pin_scoped(&req, answer.decision, answer.scope);
                }
                let resolution = match (answer.decision, answer.pinned_by_rule) {
                    (ApprovalDecision::Approve, false) => ApprovalResolution::Approved,
                    (ApprovalDecision::Reject, false) => ApprovalResolution::RejectedByUser,
                    (ApprovalDecision::Approve, true) => {
                        ApprovalResolution::PinnedByRule { approved: true }
                    }
                    (ApprovalDecision::Reject, true) => {
                        ApprovalResolution::PinnedByRule { approved: false }
                    }
                };
                // The scope is reported even for `once`, so the audit record
                // always states how far the decision was meant to reach.
                ApprovalOutcome {
                    resolution,
                    scope: Some(answer.scope),
                }
            },
        );

        // The turn is resuming — but light the session back to `Running` only
        // when no ask of this session is still pending: with parallel asks
        // outstanding the first answer must not flap the status while the rest
        // still wait. A poisoned lock keeps the safer `AwaitingApproval`.
        let no_pending = self.pending.lock().map_or(false, |p| p.is_empty());
        self.publish(if no_pending {
            ActivityStatus::Running
        } else {
            ActivityStatus::AwaitingApproval
        });
        outcome
    }

    /// The gateway routes decisions over the shared [`PendingApprovals`] table,
    /// so any number of requests can be parked at once — enabling the agent's
    /// two-phase dispatch (`doc/permission.md` §5).
    fn supports_concurrent_requests(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Build a gate plus the two halves a test drives by hand: the shared
    /// pending table (the actor's side) and the live policy the agent would
    /// evaluate against.
    fn test_gate(
        on_scoped: Option<Arc<dyn Fn(ScopedDecision) + Send + Sync>>,
    ) -> (
        GatewayApprovalGate,
        PendingApprovals,
        Arc<RwLock<PermissionPolicy>>,
    ) {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let policy = Arc::new(RwLock::new(PermissionPolicy::default()));
        let (outbound, _rx) = broadcast::channel(8);
        let gate = GatewayApprovalGate::new(
            Arc::clone(&pending),
            outbound,
            StatusHub::new(),
            SessionId("s1".to_owned()),
            WorkspaceId::none(),
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&policy),
            on_scoped,
        );
        (gate, pending, policy)
    }

    fn req(tool_name: &str, input: serde_json::Value, call_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool_name: tool_name.to_owned(),
            input,
            call_id: call_id.to_owned(),
        }
    }

    /// The actor's half of the loop: wait for the gate to park its waiter, then
    /// deliver a human decision. Polled concurrently with `request` via
    /// `join!`, so no `Arc` gymnastics are needed.
    async fn deliver(
        pending: &PendingApprovals,
        call_id: &str,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let waiter = pending.lock().unwrap().remove(call_id);
            if let Some(entry) = waiter {
                entry
                    .sender
                    .send(PendingAnswer {
                        decision,
                        scope,
                        pinned_by_rule: false,
                    })
                    .unwrap();
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "gate never parked a waiter"
            );
            tokio::task::yield_now().await;
        }
    }

    /// Session scope approves pin an `allow` rule compiled from the call and
    /// return the scope for the audit — the pinned rule takes effect on the
    /// next evaluation of the same live policy.
    #[tokio::test]
    async fn session_scope_approve_pins_allow_rule() {
        let (gate, pending, policy) = test_gate(None);
        let (outcome, ()) = tokio::join!(
            gate.request(req(
                "shell",
                serde_json::json!({"command": "cargo test"}),
                "c1"
            )),
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Approve,
                ApprovalScope::Session
            ),
        );
        assert_eq!(outcome.resolution, ApprovalResolution::Approved);
        assert_eq!(outcome.scope, Some(ApprovalScope::Session));
        let expected = rule_from_call(
            "shell",
            &serde_json::json!({"command": "cargo test"}),
            Some("command"),
        );
        assert_eq!(policy.read().unwrap().allow, vec![expected]);
        assert!(policy.read().unwrap().deny.is_empty());
    }

    /// `once` is the resting case: the call runs (or not) but nothing is
    /// pinned — the policy stays empty.
    #[tokio::test]
    async fn once_scope_pins_nothing() {
        let (gate, pending, policy) = test_gate(None);
        let (outcome, ()) = tokio::join!(
            gate.request(req("shell", serde_json::json!({"command": "ls"}), "c1")),
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Approve,
                ApprovalScope::Once
            ),
        );
        assert_eq!(outcome.resolution, ApprovalResolution::Approved);
        // Even `once` reports its scope, so the audit states the intended reach.
        assert_eq!(outcome.scope, Some(ApprovalScope::Once));
        assert!(policy.read().unwrap().is_empty());
    }

    /// A scoped *rejection* pins into `deny`, not `allow` — "never do this
    /// again" must harden the policy, not loosen it.
    #[tokio::test]
    async fn session_scope_reject_pins_deny_rule() {
        let (gate, pending, policy) = test_gate(None);
        let (outcome, ()) = tokio::join!(
            gate.request(req("write", serde_json::json!({"path": "/etc/x"}), "c1")),
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Reject,
                ApprovalScope::Session
            ),
        );
        assert_eq!(outcome.resolution, ApprovalResolution::RejectedByUser);
        assert_eq!(policy.read().unwrap().deny.len(), 1);
        assert_eq!(policy.read().unwrap().deny[0].tool, "write");
        assert_eq!(
            policy.read().unwrap().deny[0].field.as_deref(),
            Some("path")
        );
        assert!(policy.read().unwrap().allow.is_empty());
    }

    /// Pinning the same call twice (e.g. the model retries and the human
    /// re-approves) must not grow the list — the dedup is what makes repeated
    /// layering idempotent.
    #[tokio::test]
    async fn pinning_dedups_repeat_decisions() {
        let (gate, pending, policy) = test_gate(None);
        for call_id in ["c1", "c2"] {
            let ((), ()) = tokio::join!(
                async {
                    let outcome = gate
                        .request(req(
                            "shell",
                            serde_json::json!({"command": "make"}),
                            call_id,
                        ))
                        .await;
                    assert_eq!(outcome.resolution, ApprovalResolution::Approved);
                },
                deliver(
                    &pending,
                    call_id,
                    ApprovalDecision::Approve,
                    ApprovalScope::Session
                ),
            );
        }
        assert_eq!(policy.read().unwrap().allow.len(), 1);
    }

    /// A `profile`/`gateway` scope ALSO fires the persistence callback with the
    /// compiled rule — the registry half of the pin. The live policy is updated
    /// regardless; persistence is additive.
    #[tokio::test]
    async fn profile_scope_fires_persistence_callback() {
        let captured: Arc<Mutex<Vec<ScopedDecision>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let (gate, pending, policy) = test_gate(Some(Arc::new(move |sd| {
            sink.lock().unwrap().push(sd);
        })));
        let (outcome, ()) = tokio::join!(
            gate.request(req(
                "shell",
                serde_json::json!({"command": "git status"}),
                "c1"
            )),
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Approve,
                ApprovalScope::Profile
            ),
        );
        assert_eq!(outcome.scope, Some(ApprovalScope::Profile));
        // Pinned live AND handed to the persister.
        assert_eq!(policy.read().unwrap().allow.len(), 1);
        assert_eq!(captured.lock().unwrap().len(), 1);
        assert_eq!(
            captured.lock().unwrap()[0].decision,
            ApprovalDecision::Approve
        );
        assert_eq!(captured.lock().unwrap()[0].scope, ApprovalScope::Profile);
        assert_eq!(
            captured.lock().unwrap()[0].rule.patterns,
            vec!["git status".to_owned()]
        );
    }

    /// A tool outside the built-in catalog (an MCP tool) has no primary field:
    /// the pinned rule degrades to a bare tool-level rule.
    #[tokio::test]
    async fn unknown_tool_pins_bare_rule() {
        let (gate, pending, policy) = test_gate(None);
        let ((), ()) = tokio::join!(
            async {
                gate.request(req("mcp__search", serde_json::json!({"q": "x"}), "c1"))
                    .await;
            },
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Approve,
                ApprovalScope::Session
            ),
        );
        assert_eq!(
            policy.read().unwrap().allow,
            vec![Rule::contains("mcp__search", Vec::new())]
        );
    }

    /// A dropped waiter (cancel/shutdown, the actor's `clear_pending`) resolves
    /// fail-closed with no scope — no human decided, and the audit must say so.
    #[tokio::test]
    async fn dropped_waiter_is_fail_closed() {
        let (gate, pending, policy) = test_gate(None);
        let (outcome, ()) = tokio::join!(
            gate.request(req("shell", serde_json::json!({"command": "ls"}), "c1")),
            async {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    // Remove-and-drop the sender, exactly what `clear_pending` does.
                    if pending.lock().unwrap().remove("c1").is_some() {
                        return;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "gate never parked a waiter"
                    );
                    tokio::task::yield_now().await;
                }
            },
        );
        assert_eq!(outcome.resolution, ApprovalResolution::AutoDenied);
        assert_eq!(outcome.scope, None);
        assert!(policy.read().unwrap().is_empty());
    }

    /// A scoped pin re-evaluates the asks still parked: a `session` allow pin
    /// auto-approves a matching pending ask — resolved as the rule's decision
    /// (`PinnedByRule`), not a fresh human answer (`doc/permission.md` §5.1).
    #[tokio::test]
    async fn allow_pin_auto_approves_matching_pending_asks() {
        let (gate, pending, policy) = test_gate(None);
        let (out1, out2, ()) = tokio::join!(
            gate.request(req(
                "shell",
                serde_json::json!({"command": "cargo test"}),
                "c1"
            )),
            gate.request(req(
                "shell",
                serde_json::json!({"command": "cargo test"}),
                "c2"
            )),
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Approve,
                ApprovalScope::Session
            ),
        );
        assert_eq!(out1.resolution, ApprovalResolution::Approved);
        assert_eq!(out1.scope, Some(ApprovalScope::Session));
        assert_eq!(
            out2.resolution,
            ApprovalResolution::PinnedByRule { approved: true },
            "the matching parked ask is auto-approved by the pinned rule"
        );
        assert_eq!(out2.scope, Some(ApprovalScope::Session));
        assert_eq!(policy.read().unwrap().allow.len(), 1);
    }

    /// The deny side: a `session` reject pin auto-rejects matching parked asks
    /// (`PinnedByRule { approved: false }`) — the failure is a policy deny,
    /// never misattributed to a user rejection.
    #[tokio::test]
    async fn reject_pin_auto_denies_matching_pending_asks() {
        let (gate, pending, _policy) = test_gate(None);
        let (_out1, out2, ()) = tokio::join!(
            gate.request(req(
                "shell",
                serde_json::json!({"command": "rm -rf /"}),
                "c1"
            )),
            gate.request(req(
                "shell",
                serde_json::json!({"command": "rm -rf /"}),
                "c2"
            )),
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Reject,
                ApprovalScope::Session
            ),
        );
        assert_eq!(
            out2.resolution,
            ApprovalResolution::PinnedByRule { approved: false }
        );
        assert_eq!(out2.scope, Some(ApprovalScope::Session));
    }

    /// A parked ask the new rules do NOT decide stays parked for a human —
    /// the pin must never silently resolve a call it does not cover.
    #[tokio::test]
    async fn pin_leaves_undecided_asks_parked() {
        let (gate, pending, policy) = test_gate(None);
        // Ask on every shell call, so a call the pin does not cover still
        // evaluates to `Ask` (with an empty policy everything else is `Allow`
        // and would correctly auto-approve — not what this test pins down).
        policy
            .write()
            .unwrap()
            .ask
            .push(Rule::contains("shell", Vec::new()));
        let (out1, out3, (), ()) = tokio::join!(
            gate.request(req(
                "shell",
                serde_json::json!({"command": "cargo test"}),
                "c1"
            )),
            gate.request(req("shell", serde_json::json!({"command": "make"}), "c3")),
            async {
                // Wait for the pin to land (the policy gains the allow rule),
                // then check c3 is still parked before cleaning up.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    if !policy.read().unwrap().allow.is_empty() {
                        break;
                    }
                    assert!(std::time::Instant::now() < deadline, "pin never landed");
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    pending.lock().unwrap().len(),
                    1,
                    "c3 stays parked — the pin does not decide it"
                );
                // Cleanup: resolve c3 fail-closed so nothing leaks.
                pending.lock().unwrap().clear();
            },
            deliver(
                &pending,
                "c1",
                ApprovalDecision::Approve,
                ApprovalScope::Session
            ),
        );
        assert_eq!(out1.resolution, ApprovalResolution::Approved);
        assert_eq!(
            out3.resolution,
            ApprovalResolution::AutoDenied,
            "c3 fell out via the cleanup clear, not via the pin"
        );
    }
}
