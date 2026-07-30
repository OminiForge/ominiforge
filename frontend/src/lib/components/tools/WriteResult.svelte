<script lang="ts">
	import Diff from './Diff.svelte';
	import CodeView from './CodeView.svelte';
	import RawArgs from './RawArgs.svelte';
	import { parseView } from '$lib/tools/view';

	/** `write` result: rendered from the backend's UI view (`doc/tool-view.md`).
	 *  An overwrite's view is the exact old→new unified diff (rendered via
	 *  `Diff`); a new file's view is the full content (rendered via `CodeView`,
	 *  not a diff — there is no "before" side). `result` is only the terse
	 *  confirmation (`wrote PATH (new, N lines)` / `(~, +A -B)` / `(no change)`),
	 *  so we parse its first line for the meta and move the confirmation itself
	 *  to the debug fold. A failure's message (`write_failed`) is diagnostic
	 *  detail and stays in the primary view. While running, `view` is the stage-2
	 *  streaming snapshot (a growing code view or diff), flushed complete at
	 *  BlockStop — so an awaiting-approval card already shows the full change. */
	let {
		args,
		result,
		diagnostics,
		status,
		view,
		pending
	}: {
		args: string;
		result?: string;
		diagnostics?: string;
		status: 'running' | 'done' | 'error';
		view?: string;
		/** True while the call awaits a permission decision: the badge reads
		 *  "awaiting approval". The card's `view` already shows the change. */
		pending?: boolean;
	} = $props();

	const parsed = $derived(view ? parseView(view) : null);

	const errorText = $derived(status === 'error' ? result : undefined);

	// A diff (overwrite) carries `kind: "diff"`; a new file's view is
	// `kind: "code"`.
	const files = $derived(parsed?.kind === 'diff' ? parsed.files : []);
	const code = $derived(parsed?.kind === 'code' ? parsed : null);
</script>

<div class="result">
	<div class="sum">
		<span class="verb" class:running={status === 'running'} class:error={status === 'error'}>
			{pending ? 'awaiting approval' : status === 'running' ? 'writing' : status === 'error' ? 'write failed' : 'wrote'}
		</span>
	</div>
	{#if errorText}<div class="err">{errorText}</div>{/if}
	{#if files.length}
		{#each files as f (f.path)}
			<Diff text={f.patch} />
		{/each}
	{:else if code}
		<CodeView code={code.content} path={code.path} />
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
	.err {
		color: var(--state-error-text);
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
