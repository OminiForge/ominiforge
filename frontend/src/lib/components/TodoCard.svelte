<script lang="ts">
	import type { TodoStep } from '$lib/conversation';

	/** The todo checklist card: collapsible header (icon + progress bar) over
	 *  the step list. Renders both the inline history card and the sticky dock
	 *  above the input (`pinned` swaps the radius/border so the dock reads as
	 *  part of the composer zone). The caller owns the collapse state (a
	 *  per-item override map inline; a single flag in the dock) and gets flips
	 *  via `onToggle`. */
	let {
		steps,
		expanded,
		pinned = false,
		onToggle
	}: {
		steps: TodoStep[];
		expanded: boolean;
		pinned?: boolean;
		onToggle: () => void;
	} = $props();

	/** Whether every step has reached a terminal state. An empty todo list is not
	 *  "done" — it is a placeholder still being established. */
	function todoDone(steps: TodoStep[]): boolean {
		return steps.length > 0 && steps.every((s) => isTerminal(s.status));
	}

	function isTerminal(status: TodoStep['status']): boolean {
		return status === 'completed' || status === 'cancelled' || status === 'blocked';
	}

	/** Resolved-step count over total, for the todo header progress. Cancelled
	 *  counts as resolved (the step was objectively unreachable and dealt with),
	 *  so the bar only stays short while a step is still pending/in_progress or
	 *  BLOCKED — i.e. a sub-100% bar signals a step is waiting on the user, not
	 *  merely cancelled. See StepStatus in `src/agent/todo.rs`. */
	const prog = $derived.by(() => {
		const done = steps.filter((s) => s.status === 'completed' || s.status === 'cancelled').length;
		return { done, total: steps.length };
	});
</script>

<div class="plan-card" class:pinned class:expanded class:done={todoDone(steps)}>
	<button class="plan-head" onclick={onToggle} aria-expanded={expanded}>
		<svg
			class="plan-icon"
			viewBox="0 0 14 14"
			fill="none"
			stroke="currentColor"
			stroke-width="1.5"
			stroke-linecap="round"
			stroke-linejoin="round"
		>
			<path d="M3 3h8M3 7h8M3 11h5" />
		</svg>
		<span class="plan-title">Todo</span>
		<span class="plan-progress">{prog.done}/{prog.total}</span>
		<span class="plan-track"
			><span class="plan-bar" style="width: {prog.total ? (prog.done / prog.total) * 100 : 0}%"
			></span></span
		>
		<svg
			class="plan-chevron"
			viewBox="0 0 12 12"
			fill="none"
			stroke="currentColor"
			stroke-width="1.6"
			stroke-linecap="round"
			stroke-linejoin="round"
		>
			<polyline points="4,2 8,6 4,10" />
		</svg>
	</button>
	{#if expanded}
		<ol class="plan-steps">
			{#each steps as step (step.id)}
				<li class="plan-step" data-status={step.status}>
					<span class="plan-step-mark" aria-hidden="true">
						{#if step.status === 'completed'}
							<svg
								viewBox="0 0 12 12"
								fill="none"
								stroke="currentColor"
								stroke-width="2"
								stroke-linecap="round"
								stroke-linejoin="round"><polyline points="2.5,6.5 5,9 9.5,3.5" /></svg
							>
						{:else if step.status === 'in_progress'}
							<span class="plan-spinner"></span>
						{:else if step.status === 'cancelled'}
							<svg
								viewBox="0 0 12 12"
								fill="none"
								stroke="currentColor"
								stroke-width="1.8"
								stroke-linecap="round"
								><line x1="3" y1="3" x2="9" y2="9" /><line x1="9" y1="3" x2="3" y2="9" /></svg
							>
						{:else if step.status === 'blocked'}
							<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6"
								><circle cx="6" cy="6" r="4.2" /><line
									x1="3"
									y1="3"
									x2="9"
									y2="9"
									stroke-linecap="round"
								/></svg
							>
						{:else}
							<span class="plan-dot"></span>
						{/if}
					</span>
					<span class="plan-step-body">
						<span class="plan-step-text">{step.content}</span>
						{#if step.reason}
							<span class="plan-step-reason">{step.reason}</span>
						{/if}
					</span>
				</li>
			{/each}
		</ol>
	{/if}
</div>

<style>
	/* ---- TODO CARD (inline checklist + sticky dock) ---- */
	/* Neutral surface, tool-block family — the indigo accent is rationed to the
	   Todo label/icon only (like reasoning's indigo label), so the card blends
	   into the conversation and the input dock rather than shouting. */
	.plan-card {
		border-radius: var(--radius-md);
		overflow: hidden;
		border: 1px solid var(--border-subtle);
		background: var(--canvas-overlay);
		transition: border-color var(--dur-std) var(--ease-out);
	}

	/* Sticky dock variant: matches the input box — same radius/surface/border so
	   the running todo list reads as part of the composer zone. */
	.plan-card.pinned {
		border-radius: var(--radius-lg);
		border-color: var(--border-default);
	}

	.plan-head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 7px var(--space-3);
		width: 100%;
		text-align: left;
		background: transparent;
		cursor: pointer;
		user-select: none;
		transition: background var(--dur-fast) var(--ease-out);
	}

	button.plan-head:hover {
		background: color-mix(in srgb, var(--plan-accent) 8%, transparent);
	}

	.plan-icon {
		width: 13px;
		height: 13px;
		flex-shrink: 0;
		color: var(--plan-accent);
	}

	.plan-title {
		font-size: 10.5px;
		font-weight: 510;
		color: var(--plan-accent);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		flex-shrink: 0;
	}

	.plan-progress {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--text-tertiary);
		font-variant-numeric: tabular-nums;
		flex-shrink: 0;
	}

	/* Progress track: inline/dock use remaining flex width in the head row. */
	.plan-track {
		flex: 1;
		height: 4px;
		background: var(--canvas-float);
		border-radius: var(--radius-sm);
		overflow: hidden;
	}

	.plan-bar {
		display: block;
		height: 100%;
		background: var(--plan-accent);
		border-radius: var(--radius-sm);
		transition: width var(--dur-std) var(--ease-out);
	}

	.plan-chevron {
		width: 12px;
		height: 12px;
		color: var(--text-tertiary);
		transition: transform var(--dur-std) var(--ease-out);
		flex-shrink: 0;
	}
	.plan-card.expanded .plan-chevron {
		transform: rotate(90deg);
	}

	.plan-steps {
		list-style: none;
		padding: var(--space-2) var(--space-3) var(--space-3);
		margin: 0;
		display: grid;
		gap: var(--space-1);
		border-top: 1px solid var(--border-subtle);
	}

	.plan-step {
		display: grid;
		grid-template-columns: 14px 1fr;
		gap: var(--space-2);
		align-items: start;
		padding: 3px 0;
	}

	.plan-step-mark {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 14px;
		height: 16px; /* align icon to the first text line */
		flex-shrink: 0;
		color: var(--text-tertiary);
	}
	.plan-step-mark svg {
		width: 12px;
		height: 12px;
	}

	.plan-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		border: 1.5px solid var(--text-tertiary);
	}

	/* Per-status colours: in_progress=running amber, completed=done green,
	   blocked=error red (needs user), cancelled=muted+struck. */
	.plan-step[data-status='in_progress'] .plan-step-mark {
		color: var(--state-running);
	}
	.plan-step[data-status='completed'] .plan-step-mark {
		color: var(--state-done);
	}
	.plan-step[data-status='blocked'] .plan-step-mark {
		color: var(--state-error);
	}
	.plan-step[data-status='cancelled'] .plan-step-mark {
		color: var(--text-disabled);
	}

	.plan-step-body {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}

	.plan-step-text {
		font-size: 12.5px;
		line-height: 1.45;
		color: var(--text-secondary);
		font-family: var(--font-chinese);
		text-wrap: pretty;
		word-break: break-word;
	}
	.plan-step[data-status='in_progress'] .plan-step-text {
		color: var(--text-primary);
		font-weight: 500;
	}
	.plan-step[data-status='completed'] .plan-step-text {
		color: var(--text-tertiary);
	}
	.plan-step[data-status='cancelled'] .plan-step-text {
		color: var(--text-disabled);
		text-decoration: line-through;
	}

	.plan-step-reason {
		font-size: 11px;
		line-height: 1.4;
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
	}
	.plan-step[data-status='blocked'] .plan-step-reason {
		color: var(--state-error-text);
	}

	.plan-spinner {
		width: 10px;
		height: 10px;
		border: 1.5px solid color-mix(in srgb, var(--state-running) 25%, transparent);
		border-top-color: var(--state-running);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
	}

	/* Spinner rotation (plan steps). */
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.plan-spinner {
			animation: none;
		}
	}
</style>
