<script lang="ts">
	// Session activity status icon for the session list. Pure CSS/SVG, token-driven
	// (DESIGN.md §5 bans emoji; §1.3 wants status expressed redundantly by
	// color + shape + motion). Four states, each visually distinct at ~11px:
	//   running  → amber ring spinner (rotation)      — "working"
	//   awaiting → amber pulsing dot (no rotation)     — "blocked, needs you"
	//   unseen   → solid acid-lime dot (one-shot fade) — "finished, look here"
	//   seen     → muted hollow check (no motion)      — resting
	import type { ViewState } from '$lib/status.svelte';

	let { state }: { state: ViewState } = $props();

	const label: Record<ViewState, string> = {
		running: '运行中',
		awaiting: '待审批',
		unseen: '已完成未查看',
		seen: '已完成'
	};
</script>

<span
	class="status-icon"
	class:running={state === 'running'}
	class:awaiting={state === 'awaiting'}
	class:unseen={state === 'unseen'}
	class:seen={state === 'seen'}
	role="img"
	aria-label={label[state]}
	title={label[state]}
>
	{#if state === 'running'}
		<span class="spinner"></span>
	{:else if state === 'awaiting'}
		<span class="dot pulse"></span>
	{:else if state === 'unseen'}
		<span class="dot solid"></span>
	{:else}
		<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" aria-hidden="true">
			<polyline points="2.5,6.5 5,9 9.5,3.5" />
		</svg>
	{/if}
</span>

<style>
	.status-icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 12px;
		height: 12px;
		flex-shrink: 0;
	}

	/* running: amber ring spinner — same recipe as Conversation's .plan-spinner. */
	.spinner {
		width: 11px;
		height: 11px;
		border: 1.5px solid color-mix(in srgb, var(--state-running) 25%, transparent);
		border-top-color: var(--state-running);
		border-radius: 50%;
		animation: status-spin 700ms linear infinite;
	}

	.dot {
		width: 5px;
		height: 5px;
		border-radius: 50%;
	}

	/* awaiting: amber, pulsing (attention) but NOT spinning — reads as "waiting". */
	.awaiting .dot {
		background: var(--state-running);
	}
	.pulse {
		animation: status-pulse 1s ease-in-out infinite;
	}

	/* unseen: the one scarce acid-lime accent, drawing the eye. One-shot fade-in on
	   appearance; no infinite motion (§1.2 rations the accent, §5 avoids churn). */
	.unseen .dot.solid {
		background: var(--accent);
		animation: status-appear 200ms var(--ease-out);
	}

	/* seen: muted hollow check — resting, no motion. */
	.seen svg {
		width: 11px;
		height: 11px;
		color: var(--state-done);
		stroke-width: 1.6;
		stroke-linecap: round;
		stroke-linejoin: round;
		opacity: 0.7;
	}

	@keyframes status-spin {
		to {
			transform: rotate(360deg);
		}
	}
	@keyframes status-pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.35;
		}
	}
	@keyframes status-appear {
		from {
			opacity: 0;
			transform: scale(0.6);
		}
		to {
			opacity: 1;
			transform: scale(1);
		}
	}

	/* Respect reduced motion: freeze the animations (color + shape still convey
	   state). tokens.css zeroes the motion vars; the component's own keyframes must
	   be gated explicitly, as Conversation.svelte does. */
	@media (prefers-reduced-motion: reduce) {
		.spinner,
		.pulse,
		.unseen .dot.solid {
			animation: none;
		}
	}
</style>
