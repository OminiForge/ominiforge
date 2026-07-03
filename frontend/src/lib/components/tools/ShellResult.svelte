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

	/** Extract the command string from the shell tool's JSON args. */
	const command = $derived.by(() => {
		try {
			const obj = JSON.parse(args) as Record<string, unknown>;
			for (const k of ['command', 'cmd', 'script']) {
				if (typeof obj[k] === 'string' && obj[k]) return obj[k] as string;
			}
		} catch { /* partial or invalid JSON */ }
		return null;
	});
</script>

<div class="result">
	{#if command}
		<div class="cmd">{command}</div>
	{/if}
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
	.cmd {
		color: var(--accent-ink);
		font-weight: 500;
		line-height: 1.5;
		margin-bottom: var(--space-2);
		padding-bottom: var(--space-2);
		border-bottom: 1px solid var(--border-subtle);
		word-break: break-all;
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
