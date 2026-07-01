<script lang="ts">
	/** Renders a unified-diff body (the `@@`/` `/`-`/`+` lines that `edit` and
	 *  `write` emit) as coloured rows. Parsing to typed rows + `{#each}` keeps
	 *  this off the `{@html}` sink entirely — no escaping, no injection surface.
	 *
	 *  `text` is the diff body only (the caller strips the summary header line).
	 *  Blank input renders nothing. */
	let { text }: { text: string } = $props();

	type Kind = 'add' | 'del' | 'ctx' | 'hunk';
	interface Row {
		kind: Kind;
		text: string;
	}

	function classify(line: string): Kind {
		if (line.startsWith('@@')) return 'hunk';
		if (line[0] === '+') return 'add';
		if (line[0] === '-') return 'del';
		return 'ctx';
	}

	const rows = $derived<Row[]>(
		text
			.split('\n')
			.filter((l) => l !== '')
			.map((l) => ({ kind: classify(l), text: l }))
	);
</script>

<div class="diff">
	{#each rows as row (row)}
		<div class="dl {row.kind}">{row.text}</div>
	{/each}
</div>

<style>
	/* Unified diff block: monospace rows, per-kind tint + a thin left rail so
	   added/removed scan at a glance without a heavy full-row fill. */
	.diff {
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-sm);
		overflow-x: auto;
		background: var(--canvas-base);
		font-family: var(--font-mono);
		font-size: 11.5px;
		line-height: 1.6;
	}
	.dl {
		white-space: pre;
		padding: 0 var(--space-2);
		border-left: 2px solid transparent;
	}
	.dl.add {
		color: var(--state-done-text);
		background: var(--state-done-bg);
		border-left-color: var(--state-done);
	}
	.dl.del {
		color: var(--state-error-text);
		background: var(--state-error-bg);
		border-left-color: var(--state-error);
	}
	.dl.ctx {
		color: var(--text-tertiary);
	}
	.dl.hunk {
		color: var(--accent-ink);
		background: var(--canvas-overlay);
		user-select: none;
	}
</style>
