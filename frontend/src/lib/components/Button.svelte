<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		onclick,
		disabled = false,
		variant = 'default',
		type = 'button',
		children
	}: {
		onclick?: () => void;
		disabled?: boolean;
		variant?: 'default' | 'accent' | 'ghost';
		type?: 'button' | 'submit';
		children: Snippet;
	} = $props();
</script>

<button class="btn {variant}" {type} {disabled} {onclick}>
	{@render children()}
</button>

<style>
	.btn {
		display: inline-flex;
		align-items: center;
		gap: var(--gap-sm);
		padding: var(--gap-sm) var(--gap-lg);
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text-primary);
		font-size: 14px;
		font-weight: 500;
		border-radius: var(--radius-md);
		transition: all var(--motion-fast);
		cursor: pointer;
	}

	.btn:hover:not(:disabled) {
		background: var(--surface-hover);
		border-color: var(--border-hover);
	}

	.btn:active:not(:disabled) {
		transform: scale(0.98);
	}

	.btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	.btn.accent {
		background: var(--accent);
		border-color: var(--accent);
		color: var(--accent-fg);
	}

	.btn.accent:hover:not(:disabled) {
		background: var(--accent-hover);
		border-color: var(--accent-hover);
	}

	.btn.ghost {
		background: transparent;
		border-color: transparent;
		color: var(--text-secondary);
	}

	.btn.ghost:hover:not(:disabled) {
		background: var(--surface);
		color: var(--text-primary);
	}
</style>
