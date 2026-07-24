<script lang="ts">
	import Diff from './Diff.svelte';
	import CodeView from './CodeView.svelte';
	import RawArgs from './RawArgs.svelte';
	import { splitViewFiles } from '$lib/tools/view';
	import { extractArgsPath } from '$lib/tools/utils';

	/** `write` result: rendered from the backend's UI view (`doc/tool-view.md`).
	 *  An overwrite's view is the exact old→new unified diff (rendered via
	 *  `Diff`); a new file's view is the full content (rendered via `CodeView`,
	 *  not a diff — there is no "before" side). `result` is only the terse
	 *  confirmation (`wrote PATH (new, N lines)` / `(~, +A -B)` / `(no change)`),
	 *  so we parse its first line for the meta and move the confirmation itself
	 *  to the debug fold. A failure's message (`write_failed`) is diagnostic
	 *  detail and stays in the primary view. While running there is no view
	 *  yet — the streaming args remain visible in the debug fold. */
	let {
		args,
		result,
		diagnostics,
		status,
		view,
		preview
	}: {
		args: string;
		result?: string;
		diagnostics?: string;
		status: 'running' | 'done' | 'error';
		view?: string;
		/** The approval-gate would-be diff/content, shown while the call awaits a
		 *  human decision. Same shape as `view`. */
		preview?: string;
	} = $props();

	const path = $derived(extractArgsPath(args) ?? undefined);

	// While awaiting approval there is no executed `view` — show the gate's
	// preview. The preview's shape (diff vs raw content) is decided by its
	// headers, not by `meta` (which only exists on the executed result).
	const shown = $derived(view ?? preview);
	const isDiff = $derived(shown?.startsWith('--- a/') ?? false);

	/** The `wrote PATH (meta)` header's meta fragment, when `result` is the
	 *  success confirmation (not a business error). */
	const meta = $derived.by<string | undefined>(() => {
		if (status !== 'done') return undefined;
		const head = (result ?? '').split('\n')[0];
		const m = /^wrote .+? \((new, \d+ lines|~, \+\d+ -\d+|no change)\)$/.exec(head);
		return m?.[1];
	});
	const errorText = $derived(status === 'error' ? result : undefined);
	const pending = $derived(!view && preview !== undefined && status === 'running');

	// A diff (overwrite, executed or preview) carries `--- a/PATH` headers and is
	// split per file; a new file's shown text is raw content for CodeView.
	const files = $derived(shown && isDiff ? splitViewFiles(shown) : []);
</script>

<div class="result">
	<div class="sum">
		<span class="verb" class:running={status === 'running'} class:error={status === 'error'}>
			{pending ? 'awaiting approval' : status === 'running' ? 'writing' : status === 'error' ? 'write failed' : 'wrote'}
		</span>
		{#if path}<span class="path">{path}</span>{/if}
		{#if meta}<span class="meta">{meta}</span>{/if}
	</div>
	{#if errorText}<div class="err">{errorText}</div>{/if}
	{#if files.length}
		{#each files as f (f.path)}
			{#if f.diff}<Diff text={f.diff} />{/if}
		{/each}
	{:else if shown && !isDiff && path}
		<CodeView code={shown} {path} />
	{/if}
</div>
<RawArgs {args} result={status === 'done' ? result : undefined} {diagnostics} />

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
	.err {
		color: var(--state-error-text);
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
