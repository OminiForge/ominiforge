<script lang="ts">
	import CodeView from './CodeView.svelte';
	import RawArgs from './RawArgs.svelte';

	/** `read` result: a file body (path chip + numbered gutter + highlighted
	 *  source, via CodeView) or a directory listing (entries, sub-dirs tinted).
	 *  Both start with a `[header]` line the tool emits. Raw args are tucked into
	 *  the debug fold. */
	let { name, args, result }: { name: string; args: string; result?: string } = $props();

	interface Parsed {
		kind: 'file' | 'dir' | 'plain';
		path?: string;
		tag?: string;
		body: string[];
	}

	const parsed = $derived.by<Parsed>(() => {
		const text = result ?? '';
		const lines = text.split('\n');
		const head = /^\[(.+?)(?:#([^#\]]+))?\]$/.exec(lines[0]);
		if (!head) return { kind: 'plain', body: lines };
		const path = head[1];
		const body = lines.slice(1);
		if (path.endsWith('/')) return { kind: 'dir', path, body };
		return { kind: 'file', path, tag: head[2], body };
	});
</script>

<div class="result">
	{#if parsed.kind === 'file' && parsed.path}
		<CodeView path={parsed.path} tag={parsed.tag} lines={parsed.body} />
	{:else if parsed.kind === 'dir' && parsed.path}
		<div class="head"><span class="path">{parsed.path}</span></div>
		<ul class="dir">
			{#each parsed.body as entry (entry)}
				<li class="entry" class:is-dir={entry.endsWith('/')}>{entry}</li>
			{/each}
		</ul>
	{:else}
		<pre class="plain">{result ?? ''}</pre>
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
