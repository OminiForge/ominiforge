<script lang="ts">
	import RawArgs from './RawArgs.svelte';
	import { parseView } from '$lib/tools/view';
	import { parseAnsi } from '$lib/ansi';

	/** `shell` result: rendered from the backend's structured UI view
	 *  (`doc/tool-view.md`) — the command's combined stdout/stderr with ANSI
	 *  colors preserved (parsed to styled segments), command + exit code from
	 *  the structured envelope. An error with empty output (e.g. `exit 3`, no
	 *  stderr) must stay visible, so it falls back to showing the error code
	 *  instead of a blank body. */
	let {
		args,
		result,
		status,
		error_code,
		view
	}: {
		args: string;
		result?: string;
		status: 'running' | 'done' | 'error';
		error_code?: string;
		view?: string;
	} = $props();

	const parsed = $derived(view ? parseView(view) : null);
	const command = $derived(parsed?.kind === 'terminal' ? parsed.command : null);
	const rawOutput = $derived(parsed?.kind === 'terminal' ? parsed.output : result);
	const segments = $derived(rawOutput ? parseAnsi(rawOutput) : []);
	const hasOutput = $derived(segments.length > 0);
	const exitCode = $derived(parsed?.kind === 'terminal' ? parsed.exit_code : undefined);
</script>

<div class="result">
	{#if command}
		<div class="cmd">{command}</div>
	{/if}
	{#if hasOutput}
		<pre class="out">{#each segments as seg, i (i)}<span
					style:color={seg.fg}
					style:background-color={seg.bg}
					style:font-weight={seg.bold ? 600 : undefined}
					style:opacity={seg.dim ? 0.6 : undefined}
					style:font-style={seg.italic ? 'italic' : undefined}
					style:text-decoration={seg.underline ? 'underline' : undefined}
				>{seg.text}</span>{/each}</pre>
	{:else if status === 'error'}
		<div class="empty">
			No output · <span class="code">{error_code ?? exitCode ?? 'error'}</span>
		</div>
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
