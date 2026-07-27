<script lang="ts">
	import Diff from './Diff.svelte';
	import RawArgs from './RawArgs.svelte';
	import { parseView } from '$lib/tools/view';

	/** `edit` result: rendered from the backend's structured UI view
	 *  (`doc/tool-view.md`) — the exact unified diff the tool produced against
	 *  the real pre-edit content — NOT rebuilt client-side. `result` is only the
	 *  terse confirmation (`edited PATH (N replacements)`), so it moves to the
	 *  debug fold; a failure's message (e.g. `not_found`/`ambiguous`) is
	 *  diagnostic detail and stays in the primary view. While running there is
	 *  no view yet — the streaming args remain visible in the debug fold. */
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
		/** The approval-gate would-be diff, shown while the call awaits a human
		 *  decision (`Permission::Requested.preview`). Same shape as `view`. */
		preview?: string;
	} = $props();

	// While the card awaits approval there is no executed `view` yet — show the
	// gate's preview diff so the human approves the actual change. Once the call
	// runs, the executed `view` (identical when the file didn't change) replaces it.
	const shown = $derived(view ?? preview);
	const parsed = $derived(shown ? parseView(shown) : null);
	const files = $derived(parsed?.kind === 'diff' ? parsed.files : []);
	const errorText = $derived(status === 'error' ? result : undefined);
	const pending = $derived(!view && preview !== undefined && status === 'running');
</script>

<div class="result">
	<div class="sum">
		<span class="verb" class:running={status === 'running'} class:error={status === 'error'}>
			{pending ? 'awaiting approval' : status === 'running' ? 'editing' : status === 'error' ? 'edit failed' : 'edited'}
		</span>
	</div>
	{#if errorText}<div class="err">{errorText}</div>{/if}
	{#each files as f (f.path)}
		<div class="file">
			<div class="path">{f.path}</div>
			<Diff text={f.patch} />
		</div>
	{/each}
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
	.file {
		display: grid;
		gap: var(--space-1);
	}
	.path {
		color: var(--accent-ink);
		font-weight: 500;
	}
	.err {
		color: var(--state-error-text);
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
