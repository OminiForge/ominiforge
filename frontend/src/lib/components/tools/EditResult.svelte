<script lang="ts">
	import Diff from './Diff.svelte';
	import RawArgs from './RawArgs.svelte';
	import { buildFileDiff, parsePartialEdits, type EditEntry } from '$lib/tools/diff-builder';

	/** `edit` result: rendered from this call's own `edits` args (full or still
	 *  streaming) plus the conversation's file cache — NOT from `result`, which the
	 *  backend now returns as a terse confirmation only (`edited PATH (N
	 *  replacements)`, no diff — see `doc/tool-protocol.md` §11.4). Building the
	 *  diff from args is what lets it render incrementally as `Delta::ToolArgs`
	 *  streams in, per entry, instead of waiting for the call to finish.
	 *
	 *  The success confirmation moves to the debug fold (RawArgs) since it is
	 *  redundant with what's already rendered here. A failure's message (e.g.
	 *  `not_found`/`ambiguous`) is NOT redundant — it's diagnostic detail the
	 *  model needed to react to — so it stays in the primary view. */
	let {
		args,
		result,
		status,
		fileCache
	}: {
		args: string;
		result?: string;
		status: 'running' | 'done' | 'error';
		fileCache?: Map<string, string[]>;
	} = $props();

	interface PathDiff {
		path: string;
		diff: string;
		note?: string;
	}

	/** Group entries by path, first-seen order — mirrors `edit.rs`'s own grouping
	 *  so a multi-path call renders one block per file in the order referenced. */
	function groupByPath(entries: EditEntry[]): Array<[string, EditEntry[]]> {
		const groups: Array<[string, EditEntry[]]> = [];
		for (const e of entries) {
			const g = groups.find(([p]) => p === e.path);
			if (g) g[1].push(e);
			else groups.push([e.path, [e]]);
		}
		return groups;
	}

	const entries = $derived(parsePartialEdits(args));
	const pathDiffs = $derived<PathDiff[]>(
		groupByPath(entries).map(([path, es]) => {
			const built = buildFileDiff(fileCache?.get(path), es);
			return { path, diff: built.diff, note: built.note };
		})
	);
	const errorText = $derived(status === 'error' ? result : undefined);
</script>

<div class="result">
	<div class="sum">
		<span class="verb" class:running={status === 'running'} class:error={status === 'error'}>
			{status === 'running' ? 'editing' : status === 'error' ? 'edit failed' : 'edited'}
		</span>
	</div>
	{#if errorText}<div class="err">{errorText}</div>{/if}
	{#each pathDiffs as pd (pd.path)}
		<div class="file">
			<div class="path">{pd.path}</div>
			{#if pd.diff}<Diff text={pd.diff} />{/if}
			{#if pd.note}<div class="note">{pd.note}</div>{/if}
		</div>
	{/each}
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
	.file {
		display: grid;
		gap: var(--space-1);
	}
	.path {
		color: var(--accent-ink);
		font-weight: 500;
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
