<script lang="ts">
	import Diff from './Diff.svelte';
	import RawArgs from './RawArgs.svelte';
	import { extractArgsPath } from '$lib/tools/utils';

	/** `write` result. Three header shapes from write.rs:
	 *  - `wrote PATH (new, N lines)` + all-`+` body → whole-file addition
	 *  - `wrote PATH (~, +A -B)` + unified diff → overwrite
	 *  - `wrote PATH (no change)` → identical content, no body
	 *  An unrecognized first line is a business error (e.g. write_failed). */
	let { args, result, status }: { args: string; result?: string; status: 'running' | 'done' | 'error' } = $props();

	interface Parsed {
		ok: boolean;
		running?: boolean;
		path?: string;
		meta?: string;
		diff: string;
		error?: string;
	}

	const parsed = $derived.by<Parsed>(() => {
		const text = result ?? '';
		if (status === 'running' && !text) {
			const p = extractArgsPath(args);
			if (p) return { ok: true, running: true, path: p, diff: '' };
			return { ok: false, diff: '' };
		}
		const nl = text.indexOf('\n');
		const head = nl === -1 ? text : text.slice(0, nl);
		const body = nl === -1 ? '' : text.slice(nl + 1);
		const m = /^wrote (.+?) \((new, \d+ lines|~, \+\d+ -\d+|no change)\)$/.exec(head);
		if (!m) return { ok: false, diff: '', error: text };
		return { ok: true, path: m[1], meta: m[2], diff: body };
	});
</script>

<div class="result">
	{#if parsed.ok}
		<div class="sum">
			<span class="verb" class:running={parsed.running}>{parsed.running ? 'writing' : 'wrote'}</span>
			<span class="path">{parsed.path}</span>
			{#if parsed.meta}<span class="meta">{parsed.meta}</span>{/if}
		</div>
		{#if parsed.diff}<Diff text={parsed.diff} />{/if}
	{:else}
		<div class="err">{parsed.error}</div>
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
		display: grid;
		gap: var(--space-2);
	}
	.sum {
		display: flex;
		align-items: baseline;
		flex-wrap: wrap;
		gap: var(--space-2);
		line-height: 1.5;
	}
	.verb {
		font-size: 10px;
		font-weight: 510;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		padding: 1px 6px;
		border-radius: var(--radius-sm);
		flex-shrink: 0;
		background: var(--state-done-bg);
		color: var(--state-done-text);
		border: 1px solid color-mix(in srgb, var(--state-done) 25%, transparent);
	}
	.verb.running {
		background: var(--state-running-bg);
		color: var(--state-running-text);
		border-color: color-mix(in srgb, var(--state-running) 25%, transparent);
	}
	.path {
		color: var(--accent-ink);
		font-weight: 500;
	}
	.meta {
		color: var(--text-tertiary);
	}
	.err {
		color: var(--state-error-text);
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
