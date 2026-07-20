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
//! A client's decision arrives as a `Command::Approve { call_id, decision }`,
//! which the actor routes by removing the matching sender from the table and
//! sending the decision — waking the parked turn. If the table entry is dropped
//! instead (turn cancelled, actor shutting down), the receiver errors and the
//! gate resolves to [`ApprovalResolution::AutoDenied`]: **fail-closed**, and
//! audited as a no-human denial rather than a user rejection (`CLAUDE.md` §12).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, oneshot};

use crate::agent::{ApprovalDecision, ApprovalGate, ApprovalRequest, ApprovalResolution};
use crate::core::SessionId;

use super::actor::GatewayEvent;
use super::status::{ActivityStatus, SessionStatus, StatusHub};
use super::workspace::WorkspaceId;

/// Parked approval waiters, keyed by tool call id. Shared between the gate (which
/// inserts on `ask`) and the actor (which removes-and-sends on `Command::Approve`,
/// and clears on cancel/shutdown). A `std::sync::Mutex` is enough: every critical
/// section is a single map op with no `.await` held across the lock.
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

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
}

impl GatewayApprovalGate {
    /// Build a gate sharing the actor's `pending` table, outbound stream, status
    /// hub, and latest-seq cache.
    #[must_use]
    pub const fn new(
        pending: PendingApprovals,
        outbound: broadcast::Sender<GatewayEvent>,
        status: StatusHub,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        latest_seq: Arc<AtomicU64>,
    ) -> Self {
        Self {
            pending,
            outbound,
            status,
            session_id,
            workspace_id,
            latest_seq,
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
}

#[async_trait::async_trait]
impl ApprovalGate for GatewayApprovalGate {
    async fn request(&self, req: ApprovalRequest) -> ApprovalResolution {
        let (tx, rx) = oneshot::channel();
        // Park the waiter before announcing, so a decision can never race in
        // ahead of the table entry. A poisoned lock drops `tx` here, so `rx`
        // errors below → `AutoDenied` — fail-closed.
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(req.call_id.clone(), tx);
        }

        self.publish(ActivityStatus::AwaitingApproval);
        // Ephemeral, like a `Delta`: a client connecting after this fires learns
        // of the pending ask from the `AwaitingApproval` status, not a replay.
        let _ = self.outbound.send(GatewayEvent::ApprovalRequested {
            call_id: req.call_id,
            tool_name: req.tool_name,
            input: req.input,
        });

        // Suspend until the actor delivers a decision — or the sender is dropped
        // (cancel/shutdown/all-handles-gone → `clear_pending`). A delivered value
        // is a human decision; a dropped channel is a no-human auto-denial.
        let resolution = match rx.await {
            Ok(ApprovalDecision::Approve) => ApprovalResolution::Approved,
            Ok(ApprovalDecision::Reject) => ApprovalResolution::RejectedByUser,
            Err(_) => ApprovalResolution::AutoDenied,
        };

        // The turn is resuming — light the session back to `Running`.
        self.publish(ActivityStatus::Running);
        resolution
    }
}
