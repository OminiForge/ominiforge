<script lang="ts">
	/** Renders a unified-diff body (the `@@`/` `/`-`/`+` lines that `edit` and
	 *  `write` emit) as coloured rows with a line-number gutter. Parsing to typed
	 *  rows + `{#each}` keeps this off the `{@html}` sink entirely — no escaping,
	 *  no injection surface.
	 *
	 *  Line numbers are rebuilt from each hunk's `@@ -oldStart,oldLen +newStart,newLen
	 *  @@` header: a context line consumes both an old and a new number, `-` only an
	 *  old, `+` only a new. The gutter shows both columns (old | new) so a deletion
	 *  and an insertion align the way the file actually changed. Hunk separators and
	 *  blank input render no numbers. */
	let { text }: { text: string } = $props();

	type Kind = 'add' | 'del' | 'ctx' | 'hunk';
	interface Row {
		kind: Kind;
		text: string;
		/** Old-side line number (blank for `+` rows and hunk headers). */
		oldNo?: number;
		/** New-side line number (blank for `-` rows and hunk headers). */
		newNo?: number;
	}

	const HUNK = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;

	function parse(body: string): Row[] {
		const rows: Row[] = [];
		let oldNo = 0;
		let newNo = 0;
		for (const line of body.split('\n')) {
			if (line === '') continue;
			const h = HUNK.exec(line);
			if (h) {
				oldNo = Number(h[1]);
				newNo = Number(h[2]);
				rows.push({ kind: 'hunk', text: line });
				continue;
			}
			const c = line[0];
			if (c === '+') {
				rows.push({ kind: 'add', text: line, newNo: newNo++ });
			} else if (c === '-') {
				rows.push({ kind: 'del', text: line, oldNo: oldNo++ });
			} else {
				// Context (leading space) — consumes both sides.
				rows.push({ kind: 'ctx', text: line, oldNo: oldNo++, newNo: newNo++ });
			}
		}
		return rows;
	}

	const rows = $derived<Row[]>(parse(text));
</script>

<div class="diff">
	{#each rows as row (row)}
		{#if row.kind === 'hunk'}
			<div class="dl hunk"><span class="gutter"></span><span class="ltext">{row.text}</span></div>
		{:else}
			<div class="dl {row.kind}">
				<span class="gutter"
					><span class="no">{row.oldNo ?? ''}</span><span class="no">{row.newNo ?? ''}</span></span
				><span class="ltext">{row.text}</span>
			</div>
		{/if}
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
		display: flex;
		white-space: pre;
		padding-right: var(--space-2);
		border-left: 2px solid transparent;
	}
	/* The gutter is the two line-number columns (old | new), right-aligned and
	   non-selecting so a copy of the diff picks up only code. */
	.gutter {
		display: inline-flex;
		flex-shrink: 0;
		user-select: none;
		padding: 0 var(--space-2) 0 var(--space-1);
		color: var(--text-tertiary);
		opacity: 0.7;
	}
	.no {
		display: inline-block;
		min-width: 3ch;
		text-align: right;
	}
	.no + .no {
		margin-left: var(--space-2);
	}
	.ltext {
		white-space: pre;
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
