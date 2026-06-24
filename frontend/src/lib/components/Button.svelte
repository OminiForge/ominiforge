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
		padding: var(--gap-sm) var(--gap-md);
		border: 1px solid var(--border);
		border-radius: var(--radius-md);
		background: var(--surface);
		color: var(--text-primary);
		font-size: 13px;
		transition:
			background 0.12s ease,
			border-color 0.12s ease;
	}

	.btn:hover:not(:disabled) {
		background: var(--surface-hover);
	}

	.btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}

	.btn.accent {
		background: var(--accent-weak);
		border-color: var(--accent-weak);
		color: var(--accent-fg);
		font-weight: 600;
	}

	.btn.accent:hover:not(:disabled) {
		background: var(--accent);
		border-color: var(--accent);
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
