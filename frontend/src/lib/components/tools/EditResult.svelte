<script lang="ts">
	import Diff from './Diff.svelte';
	import RawArgs from './RawArgs.svelte';
	import { extractArgsPath } from '$lib/tools/utils';

	/** `edit` result: a `edited PATH (N ops) -> TAG` header over a unified diff.
	 *  An unrecognized first line is a business error (e.g. stale snapshot) — show
	 *  it error-tinted rather than faking a success header. */
	let { args, result, status }: { args: string; result?: string; status: 'running' | 'done' | 'error' } = $props();

	interface Parsed {
		ok: boolean;
		running?: boolean;
		path?: string;
		ops?: string;
		tag?: string;
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
		const m = /^edited (.+?) \((\d+) ops\) -> (\S+)$/.exec(head);
		if (!m) return { ok: false, diff: '', error: text };
		return { ok: true, path: m[1], ops: m[2], tag: m[3], diff: body };
	});
</script>

<div class="result">
	{#if parsed.ok}
		<div class="sum">
			<span class="verb" class:running={parsed.running}>{parsed.running ? 'editing' : 'edited'}</span>
			<span class="path">{parsed.path}</span>
			{#if parsed.ops}<span class="meta"><span class="n">{parsed.ops}</span> ops</span>{/if}
			{#if parsed.tag}<span class="tag">#{parsed.tag}</span>{/if}
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
	.meta .n {
		color: var(--syntax-num);
	}
	.tag {
		color: var(--text-tertiary);
		font-size: 10.5px;
		margin-left: auto;
	}
	.err {
		color: var(--state-error-text);
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
