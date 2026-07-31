<script lang="ts">
	import RawArgs from './RawArgs.svelte';

	/** Fallback result for any tool without a dedicated view (MCP tools, future
	 *  built-ins). Tool names are open-ended, so this catches everything the
	 *  registry doesn't map. Plain monospace output; empty-but-failed falls back
	 *  to the error code so it's never invisible. */
	let {
		args,
		result,
		status,
		error_code
	}: {
		args: string;
		result?: string;
		status: 'running' | 'done' | 'error';
		error_code?: string;
	} = $props();
</script>

<div class="result">
	{#if result}
		<pre class="out">{result}</pre>
	{:else if status === 'error'}
		<div class="empty">No output · <span class="code">{error_code ?? 'error'}</span></div>
	{/if}
</div>
<RawArgs {args} />

<style>
	.result {
		padding: var(--space-3) var(--space-4);
		font-family: var(--font-mono);
		font-size: 11.5px;
		max-height: 320px;
		overflow: auto;
	}
	.out {
		margin: 0;
		color: var(--text-secondary);
		line-height: 1.6;
		white-space: pre-wrap;
		word-break: break-word;
	}
	.empty {
		color: var(--text-tertiary);
		font-family: var(--font-chinese);
		font-size: 12px;
	}
	.code {
		color: var(--state-error-text);
		font-family: var(--font-mono);
	}
</style>
