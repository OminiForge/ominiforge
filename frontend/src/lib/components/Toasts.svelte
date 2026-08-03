<script lang="ts">
	import { fly } from 'svelte/transition';
	import { rise } from '$lib/motion';
	import { subscribeToasts, dismissToast, type Toast } from '$lib/toast';

	/** Fixed toast stack, bottom-right, above content but below modals. Mount
	 *  once near the app root; `pushToast` feeds it from anywhere. */
	let toasts = $state<Toast[]>([]);
	$effect(() => subscribeToasts((t) => (toasts = t)));

	const ICON: Record<Toast['tone'], string> = {
		success: '✓',
		error: '✕',
		info: 'i'
	};
</script>

<div class="stack" aria-live="polite">
	{#each toasts as t (t.id)}
		<div class="toast {t.tone}" role="status" transition:fly={rise(8, 160)}>
			<span class="icon" aria-hidden="true">{ICON[t.tone]}</span>
			<span class="msg">{t.message}</span>
			<button class="x" aria-label="Dismiss" onclick={() => dismissToast(t.id)}>×</button>
		</div>
	{/each}
</div>

<style>
	.stack {
		position: fixed;
		right: var(--space-5);
		bottom: var(--space-5);
		z-index: var(--z-toast);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		max-width: min(360px, calc(100vw - 2 * var(--space-5)));
		pointer-events: none; /* the stack itself never blocks clicks… */
	}

	.toast {
		pointer-events: auto; /* …but each toast is interactive (dismiss). */
		display: flex;
		align-items: flex-start;
		gap: var(--space-2);
		padding: var(--space-3) var(--space-3);
		background: var(--canvas-float);
		border: 1px solid var(--border-default);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
		font-size: 13px;
		line-height: 1.4;
		color: var(--text-primary);
	}

	.icon {
		flex: none;
		display: grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		font-size: 11px;
		font-weight: 600;
		margin-top: 1px;
	}

	.toast.success .icon {
		color: var(--state-done-text);
		background: var(--state-done-bg);
	}

	.toast.error .icon {
		color: var(--state-error-text);
		background: var(--state-error-bg);
	}

	.toast.info .icon {
		color: var(--text-secondary);
		background: var(--canvas-overlay);
	}

	.msg {
		flex: 1;
		min-width: 0;
		overflow-wrap: break-word;
	}

	.toast.error .msg {
		color: var(--state-error-text);
	}

	.x {
		flex: none;
		border: none;
		background: none;
		color: var(--text-tertiary);
		font-size: 15px;
		line-height: 1;
		cursor: pointer;
		padding: 0 2px;
		margin-top: 1px;
	}

	.x:hover {
		color: var(--text-primary);
	}
</style>
