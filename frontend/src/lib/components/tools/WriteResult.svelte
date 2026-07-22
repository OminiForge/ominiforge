<script lang="ts">
	import Diff from './Diff.svelte';
	import RawArgs from './RawArgs.svelte';
	import { buildWriteDiff } from '$lib/tools/diff-builder';
	import { extractArgsPath } from '$lib/tools/utils';

	/** `write` result: rendered from this call's own `content` arg plus the
	 *  conversation's file cache — NOT from `result`, which the backend now
	 *  returns as a terse confirmation only (`wrote PATH (new, N lines)` /
	 *  `(~, +A -B)` / `(no change)`, no body — see `doc/tool-protocol.md` §11.4).
	 *
	 *  The confirmation header still tells us definitively which of the three
	 *  cases applies (new/overwrite/no-change) — cheap and authoritative, so we
	 *  parse just that first line to pick the render mode, then build the actual
	 *  diff body ourselves:
	 *  - new: every line of `content` shown as an addition (no real "before" to
	 *    diff against — that's what "new" means).
	 *  - overwrite: a real line diff (`diff-builder.buildWriteDiff`) against
	 *    `prevLines` — the pre-write snapshot captured on this item at commit
	 *    time (see `Item.prevLines`'s doc comment for why the file cache itself
	 *    can no longer supply this by render time).
	 *  - no change: nothing to show.
	 *  While running (no result yet), only the path is known — `content` is
	 *  still-streaming partial JSON that likely doesn't even parse yet, so no
	 *  diff preview is attempted (matches `edit`'s graceful no-preview state,
	 *  just without a diff-builder cache-miss note since there's nothing to
	 *  degrade FROM).
	 *
	 *  A failure's message (e.g. `write_failed`) is NOT redundant with args — it's
	 *  diagnostic detail — so it stays in the primary view; the success
	 *  confirmation moves to the debug fold (RawArgs). */
	let {
		args,
		result,
		status,
		fileCache,
		prevLines
	}: {
		args: string;
		result?: string;
		status: 'running' | 'done' | 'error';
		fileCache?: Map<string, string[]>;
		prevLines?: string[];
	} = $props();

	interface Parsed {
		path?: string;
		meta?: string;
		diff: string;
		note?: string;
		error?: string;
	}

	function newContentLines(): string[] | undefined {
		try {
			const a = JSON.parse(args) as { content?: unknown };
			if (typeof a.content === 'string') return a.content.split('\n');
		} catch {
			// Still streaming or malformed — no content to diff yet.
		}
		return undefined;
	}

	const parsed = $derived.by<Parsed>(() => {
		const path = extractArgsPath(args) ?? undefined;
		if (status === 'running') return { path, diff: '' };

		const text = result ?? '';
		const nl = text.indexOf('\n');
		const head = nl === -1 ? text : text.slice(0, nl);
		const m = /^wrote (.+?) \((new, \d+ lines|~, \+\d+ -\d+|no change)\)$/.exec(head);
		if (!m) return { path, diff: '', error: text }; // business error (write_failed)
		const meta = m[2];

		if (meta === 'no change') return { path, meta, diff: '' };

		const newLines = newContentLines();
		if (!newLines) return { path, meta, diff: '' };

		if (meta.startsWith('new,')) {
			return { path, meta, diff: newLines.map((l) => `+${l}`).join('\n') };
		}
		// Overwrite: diff against the pre-write snapshot captured on this item.
		// Without one (the file was never read/written this session) there is no
		// "before" to diff against — show only the meta + a caveat note; rendering
		// the whole new content as `+` lines would contradict the `~, +A -B` meta
		// (which says this was an overwrite, not a new file).
		if (!prevLines) {
			return {
				path,
				meta,
				diff: '',
				note: '无上下文（该文件在本会话未被读取或写入，无法展示逐行差异）'
			};
		}
		return { path, meta, diff: buildWriteDiff(prevLines, newLines).diff };
	});
</script>

<div class="result">
	<div class="sum">
		<span class="verb" class:running={status === 'running'} class:error={status === 'error'}>
			{status === 'running' ? 'writing' : status === 'error' ? 'write failed' : 'wrote'}
		</span>
		{#if parsed.path}<span class="path">{parsed.path}</span>{/if}
		{#if parsed.meta}<span class="meta">{parsed.meta}</span>{/if}
	</div>
	{#if parsed.error}<div class="err">{parsed.error}</div>{/if}
	{#if parsed.diff}<Diff text={parsed.diff} />{/if}
	{#if parsed.note}<div class="note">{parsed.note}</div>{/if}
</div>
<RawArgs {args} result={status === 'done' ? result : undefined} />

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
	.verb.error {
		background: var(--state-error-bg);
		color: var(--state-error-text);
		border-color: color-mix(in srgb, var(--state-error) 25%, transparent);
	}
	.path {
		color: var(--accent-ink);
		font-weight: 500;
	}
	.meta {
		color: var(--text-tertiary);
	}
	.note {
		color: var(--text-tertiary);
		font-size: 10.5px;
		font-family: var(--font-chinese);
	}
	.err {
		color: var(--state-error-text);
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
