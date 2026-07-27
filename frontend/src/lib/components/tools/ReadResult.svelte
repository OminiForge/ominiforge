<script lang="ts">
	import CodeView from './CodeView.svelte';
	import RawArgs from './RawArgs.svelte';
	import { parseView } from '$lib/tools/view';

	/** `read` result: rendered from the backend's structured UI view
	 *  (`doc/tool-view.md`) — a file body (`kind: "code"`, path chip + numbered
	 *  gutter + highlighted source, via CodeView) or a directory listing
	 *  (`kind: "listing"`, entries, sub-dirs tinted). Raw args are tucked into
	 *  the debug fold. */
	let {
		name,
		args,
		result,
		diagnostics,
		status,
		view
	}: {
		name: string;
		args: string;
		result?: string;
		diagnostics?: string;
		status: 'running' | 'done' | 'error';
		view?: string;
	} = $props();

	const parsed = $derived(view ? parseView(view) : null);
</script>

<div class="result">
	{#if parsed?.kind === 'code'}
		<CodeView path={parsed.path} code={parsed.content} />
	{:else if parsed?.kind === 'listing'}
		<div class="head"><span class="path">{parsed.path}</span></div>
		<ul class="dir">
			{#each parsed.entries as entry (entry)}
				<li class="entry" class:is-dir={entry.endsWith('/')}>{entry}</li>
			{/each}
		</ul>
	{:else}
		<pre class="plain">{result ?? ''}</pre>
	{/if}
</div>
<RawArgs {args} {diagnostics} />

<style>
	.result {
		padding: var(--space-3) var(--space-4);
		font-family: var(--font-mono);
		font-size: 11.5px;
		max-height: 320px;
		overflow: auto;
	}
	.head {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
		padding-bottom: var(--space-2);
		margin-bottom: var(--space-2);
		border-bottom: 1px solid var(--border-subtle);
	}
	.path {
		color: var(--accent-ink);
		font-weight: 500;
	}
	.dir {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 1px;
	}
	.entry {
		color: var(--text-secondary);
	}
	.entry.is-dir {
		color: var(--accent-ink);
	}
	.plain {
		margin: 0;
		color: var(--text-secondary);
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
