<script lang="ts">
	import { fly } from 'svelte/transition';
	import { rise } from '$lib/motion';

	/** Inline flash notice (e.g. "Providers saved"). Auto-dismiss timing stays
	 *  with the caller — this component only owns the look and the enter/exit
	 *  transition. */
	let { message }: { message: string } = $props();
</script>

<p class="notice" role="status" transition:fly={rise(-4, 120)}>{message}</p>

<style>
	.notice {
		color: var(--accent-ink);
		background: color-mix(in srgb, var(--accent) 10%, transparent);
		padding: var(--space-2) var(--space-4);
		border-radius: var(--radius-md);
		margin-bottom: var(--space-4);
		font-size: 13px;
	}
</style>
