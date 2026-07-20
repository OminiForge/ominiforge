<script lang="ts">
	// Inline approval prompt for a permission `ask` (doc/permission.md §6,
	// frontend/DESIGN.md §4.9). A gated tool call suspends here until the user
	// approves or rejects. Token-driven, no emoji (DESIGN §5), state expressed
	// redundantly by color + shape + motion (DESIGN §1.3): pending = amber pulse
	// border, approved = green, rejected = red.

	let {
		callId,
		toolName,
		args,
		status,
		onDecide
	}: {
		callId: string;
		toolName: string;
		args: string;
		status: 'pending' | 'approved' | 'rejected';
		/** Answer the prompt; the parent calls `client.approve`. Returns a promise
		 *  that rejects if delivery fails, so this component can re-enable its
		 *  buttons instead of freezing on "处理中…". */
		onDecide: (callId: string, decision: 'approve' | 'reject') => void | Promise<void>;
	} = $props();

	let busy = $state(false);

	async function decide(decision: 'approve' | 'reject') {
		if (busy || status !== 'pending') return;
		busy = true;
		try {
			await onDecide(callId, decision);
			// Stay busy (buttons disabled) until the committed `Permission::Decided`
			// event folds the card to approved/rejected. Re-enabling here would let
			// a double-click resubmit in the gap (the actor is idempotent, but the
			// UI shouldn't invite it).
		} catch {
			// Delivery failed (dropped connection / dead session): re-enable so the
			// user can retry, rather than freezing on "处理中…". The error itself is
			// surfaced by the parent's page-level notice; we only own the button
			// state here, so swallow to avoid an unhandled rejection off the click.
			busy = false;
		}
	}

	// Syntax-tint the JSON input under review (same recipe as tools/RawArgs).
	function escapeHtml(s: string): string {
		return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
	}
	const argsHtml = $derived.by(() => {
		let pretty = args;
		try {
			pretty = JSON.stringify(JSON.parse(args), null, 2);
		} catch {
			return escapeHtml(args);
		}
		return escapeHtml(pretty)
			.replace(/&quot;([^&]*?)&quot;(\s*:)/g, '<span class="k">&quot;$1&quot;</span>$2')
			.replace(/:\s*&quot;([^&]*?)&quot;/g, ': <span class="s">&quot;$1&quot;</span>')
			.replace(/:\s*(-?\d+(?:\.\d+)?)/g, ': <span class="n">$1</span>');
	});
</script>

<div class="approval" data-status={status}>
	<div class="head">
		<span class="pip" aria-hidden="true"></span>
		<span class="eyebrow">
			{#if status === 'pending'}待审批 · AWAITING APPROVAL
			{:else if status === 'approved'}已批准 · APPROVED
			{:else}已拒绝 · REJECTED{/if}
		</span>
		<span class="tool">{toolName}</span>
	</div>

	{#if args && args !== '{}'}
		<!-- eslint-disable-next-line svelte/no-at-html-tags -->
		<pre class="args">{@html argsHtml}</pre>
	{/if}

	{#if status === 'pending'}
		<div class="actions">
			<button class="btn reject" onclick={() => decide('reject')} disabled={busy}>拒绝</button>
			<button class="btn approve" onclick={() => decide('approve')} disabled={busy}>
				{busy ? '处理中…' : '批准'}
			</button>
		</div>
	{/if}
</div>

<style>
	/* Surface: overlay card + strong hairline — this needs the user's attention,
	   but DESIGN §5 bans big shadows / colored-border-accent slop. Amber pulse
	   border (reusing the tool-running recipe) carries "waiting" redundantly. */
	.approval {
		background: var(--canvas-overlay);
		border: 1px solid var(--border-strong);
		border-radius: var(--radius-lg);
		padding: var(--space-3) var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	.approval[data-status='pending'] {
		border-color: var(--state-running);
		animation: approval-pulse 2s ease-in-out infinite;
	}
	.approval[data-status='approved'] {
		border-color: color-mix(in srgb, var(--state-done) 55%, transparent);
	}
	.approval[data-status='rejected'] {
		border-color: color-mix(in srgb, var(--state-error) 55%, transparent);
	}

	.head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}
	.pip {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		flex-shrink: 0;
		background: var(--state-running);
	}
	[data-status='approved'] .pip {
		background: var(--state-done);
	}
	[data-status='rejected'] .pip {
		background: var(--state-error);
	}
	.eyebrow {
		font-family: var(--font-mono);
		font-size: 11px;
		font-weight: 510;
		letter-spacing: 0.08em;
		color: var(--text-secondary);
	}
	.tool {
		font-family: var(--font-mono);
		font-size: 12px;
		color: var(--text-primary);
		margin-left: auto;
	}

	.args {
		margin: 0;
		padding: var(--space-2) var(--space-3);
		background: var(--canvas-float);
		border-radius: var(--radius-sm);
		font-family: var(--font-mono);
		font-size: 11.5px;
		color: var(--text-secondary);
		line-height: 1.6;
		white-space: pre;
		overflow-x: auto;
		max-height: 220px;
	}
	.args :global(.k) {
		color: var(--syntax-key);
	}
	.args :global(.s) {
		color: var(--syntax-str);
	}
	.args :global(.n) {
		color: var(--syntax-num);
	}

	.actions {
		display: flex;
		justify-content: flex-end;
		gap: var(--space-2);
	}
	.btn {
		font-family: var(--font-sans);
		font-size: 14px;
		font-weight: 590;
		padding: 8px 14px;
		border-radius: var(--radius-md);
		border: 1px solid transparent;
		cursor: pointer;
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
	/* Approve is the single acid-lime primary action for this card (DESIGN §1.2). */
	.btn.approve {
		background: var(--accent);
		color: var(--accent-fg);
	}
	.btn.approve:hover:not(:disabled) {
		background: var(--accent-hover);
	}
	/* Reject is a quiet secondary — no accent, no red fill (a button, not an alarm). */
	.btn.reject {
		background: var(--canvas-overlay);
		color: var(--text-secondary);
		border-color: var(--border-default);
	}
	.btn.reject:hover:not(:disabled) {
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	@keyframes approval-pulse {
		0%,
		100% {
			border-color: color-mix(in srgb, var(--state-running) 45%, transparent);
		}
		50% {
			border-color: var(--state-running);
		}
	}
	/* DESIGN §3.2: freeze motion under reduced-motion; color + shape still convey. */
	@media (prefers-reduced-motion: reduce) {
		.approval[data-status='pending'] {
			animation: none;
		}
	}
</style>
