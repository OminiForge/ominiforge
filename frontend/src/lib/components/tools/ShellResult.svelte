<script lang="ts">
	import RawArgs from './RawArgs.svelte';

	/** `shell` result: the command's combined stdout/stderr as plain monospace.
	 *  An error with empty output (e.g. `exit 3`, no stderr) must stay visible, so
	 *  it falls back to showing the error code instead of a blank body. */
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
		<div class="empty">无输出 · <span class="code">{error_code ?? 'error'}</span></div>
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
