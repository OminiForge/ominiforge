<script lang="ts">
	// In-card approval controls for a permission `ask` (doc/permission.md §5,
	// frontend/DESIGN.md §4.9): two orthogonal parts — a DECISION (批准 / 拒绝,
	// one click each) and a SCOPE (仅此次 / 本次会话 / 当前 profile / 所有会话,
	// an independent selector defaulting to 仅此次). The scope applies to
	// whichever decision is clicked; non-once scopes pin the decision as a rule.
	// Rendered inside the gated call's ToolBlock header.
	import type { ApprovalScope } from '$lib/types/ApprovalScope';
	import { fly } from 'svelte/transition';
	import { rise } from '$lib/motion';

	let {
		callId,
		onDecide
	}: {
		callId: string;
		/** Answer the prompt with a decision + scope; the parent calls
		 *  `client.approve`. Returns a promise that rejects if delivery fails, so
		 *  this component can re-enable its buttons instead of freezing. */
		onDecide: (
			callId: string,
			decision: 'approve' | 'reject',
			scope: ApprovalScope
		) => void | Promise<void>;
	} = $props();

	let busy = $state(false);
	let scope = $state<ApprovalScope>('once');
	let menuOpen = $state(false);
	// Fixed-position menu coordinates (the card clips absolutely-positioned
	// overflow, so the menu escapes via `position: fixed` + anchor rect).
	let menuPos = $state<{ bottom: number; right: number } | null>(null);

	const SCOPES: { value: ApprovalScope; label: string }[] = [
		{ value: 'once', label: '仅此次' },
		{ value: 'session', label: '本次会话' },
		{ value: 'profile', label: '当前 profile' },
		{ value: 'gateway', label: '所有会话' }
	];

	const scopeLabel = $derived(SCOPES.find((s) => s.value === scope)?.label ?? '仅此次');

	async function decide(decision: 'approve' | 'reject') {
		if (busy) return;
		busy = true;
		menuOpen = false;
		try {
			await onDecide(callId, decision, scope);
			// Stay busy until the committed `Permission::Decided` folds the card out
			// of pending — re-enabling here would invite a double-click resubmit in
			// the gap (the actor is idempotent, but the UI shouldn't invite it).
		} catch {
			// Delivery failed: re-enable so the user can retry. The error surfaces
			// via the parent's page-level notice; we only own button state here.
			busy = false;
		}
	}

	function toggleMenu(e: MouseEvent) {
		if (busy) return;
		if (menuOpen) {
			menuOpen = false;
			return;
		}
		const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
		menuPos = {
			bottom: window.innerHeight - rect.top + 4,
			right: window.innerWidth - rect.right
		};
		menuOpen = true;
	}

	function pickScope(value: ApprovalScope) {
		scope = value;
		menuOpen = false;
	}

	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') menuOpen = false;
	}
</script>

<svelte:window {onkeydown} />

<div class="controls">
	{#if menuOpen}
		<!-- svelte-ignore a11y_click_events_have_key_events -->
		<div class="backdrop" role="presentation" onclick={() => (menuOpen = false)}></div>
	{/if}

	<div class="anchor">
		<button
			class="scope-btn"
			class:pinned={scope !== 'once'}
			disabled={busy}
			aria-haspopup="menu"
			aria-expanded={menuOpen}
			title="作用域：决定的有效范围"
			onclick={toggleMenu}
		>
			{scopeLabel}
			<svg
				class:open={menuOpen}
				viewBox="0 0 12 12"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				stroke-linecap="round"
				stroke-linejoin="round"
			>
				<polyline points="3,4.5 6,7.5 9,4.5" />
			</svg>
		</button>
		{#if menuOpen && menuPos}
			<div
				class="menu"
				role="menu"
				style:bottom="{menuPos.bottom}px"
				style:right="{menuPos.right}px"
				transition:fly={rise(4)}
			>
				{#each SCOPES as s (s.value)}
					<button
						class="menu-item"
						class:current={scope === s.value}
						role="menuitemradio"
						aria-checked={scope === s.value}
						onclick={() => pickScope(s.value)}
					>
						{s.label}
						{#if s.value === 'session'}<span class="menu-hint">重连/重启后失效</span>{/if}
					</button>
				{/each}
			</div>
		{/if}
	</div>

	<button class="btn reject" disabled={busy} onclick={() => decide('reject')}>拒绝</button>
	<button class="btn approve" disabled={busy} onclick={() => decide('approve')}>批准</button>
</div>

<style>
	.controls {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex-shrink: 0;
	}

	.backdrop {
		position: fixed;
		inset: 0;
		z-index: 40;
	}

	.anchor {
		position: relative;
	}

	/* Scope selector: quiet ghost; tinted amber when a pinning scope is chosen,
	   so a persistent decision never hides behind default-looking chrome. */
	.scope-btn {
		display: inline-flex;
		align-items: center;
		gap: 3px;
		font-family: var(--font-chinese);
		font-size: 12px;
		padding: 3px 6px;
		background: transparent;
		color: var(--text-tertiary);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-sm);
		cursor: pointer;
		white-space: nowrap;
	}
	.scope-btn:hover:not(:disabled) {
		color: var(--text-primary);
		border-color: var(--border-strong);
	}
	.scope-btn.pinned {
		color: var(--state-running-text);
		border-color: color-mix(in srgb, var(--state-running) 35%, transparent);
	}
	.scope-btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.scope-btn svg {
		width: 9px;
		height: 9px;
		transition: transform var(--dur-fast) var(--ease-out);
	}
	.scope-btn svg.open {
		transform: rotate(180deg);
	}

	.btn {
		font-family: var(--font-chinese);
		font-size: 12px;
		font-weight: 590;
		padding: 3px var(--space-3);
		border-radius: var(--radius-sm);
		border: 1px solid transparent;
		cursor: pointer;
		white-space: nowrap;
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}

	/* Approve is the single acid-lime primary action (DESIGN §1.2). */
	.btn.approve {
		background: var(--accent);
		color: var(--accent-fg);
	}
	.btn.approve:hover:not(:disabled) {
		background: var(--accent-hover);
	}

	/* Reject is a quiet secondary — no accent, no red fill (a button, not an alarm). */
	.btn.reject {
		background: transparent;
		color: var(--text-secondary);
		border-color: var(--border-default);
	}
	.btn.reject:hover:not(:disabled) {
		color: var(--text-primary);
		border-color: var(--border-strong);
	}

	.menu {
		position: fixed;
		z-index: 41;
		min-width: 112px;
		display: flex;
		flex-direction: column;
		padding: 3px;
		background: var(--canvas-float);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-md);
	}

	.menu-item {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: var(--space-2);
		padding: 5px var(--space-3);
		background: transparent;
		border: none;
		border-radius: var(--radius-sm);
		color: var(--text-secondary);
		font-family: var(--font-chinese);
		font-size: 12px;
		text-align: left;
		white-space: nowrap;
		cursor: pointer;
	}

	.menu-hint {
		font-size: 11px;
		color: var(--text-tertiary);
	}

	.menu-item:hover {
		background: var(--surface-hover);
		color: var(--text-primary);
	}

	.menu-item.current {
		color: var(--text-primary);
		font-weight: 590;
	}
</style>
