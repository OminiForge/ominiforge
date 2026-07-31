<script lang="ts">
	/** Debug-only view of a tool call's raw JSON arguments, and — when the caller
	 *  passes one — its terse result text and/or LSP diagnostics. Collapsed by
	 *  default: the tool-specific header already surfaces what matters
	 *  (path/command/diff/…); this is here for when you need to inspect the exact
	 *  call. `result` is optional because most tools still render it as their
	 *  primary content — only `edit`/`write` (whose result is now a terse
	 *  confirmation, not a diff — see `doc/tool-protocol.md` §11.4) pass it here
	 *  instead. `diagnostics` is the LSP block a tool appended (`doc/lsp.md` §5): it went to
	 *  the model, so it's shown here for transparency — never in the primary view,
	 *  where it would pollute the file body / diff. Pretty-prints + syntax-tints
	 *  JSON; falls back to escaped raw text when a value isn't valid JSON (true
	 *  for `result`/`diagnostics`, which are always plain text). Only the tint
	 *  markup (built from escaped text) reaches `{@html}`. */
	let {
		args,
		result,
		diagnostics
	}: { args: string; result?: string; diagnostics?: string } = $props();

	let open = $state(false);

	function escapeHtml(s: string): string {
		return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
	}

	function tintJson(text: string): string {
		let pretty = text;
		try {
			pretty = JSON.stringify(JSON.parse(text), null, 2);
		} catch {
			return escapeHtml(text);
		}
		return escapeHtml(pretty)
			.replace(/&quot;([^&]*?)&quot;(\s*:)/g, '<span class="k">&quot;$1&quot;</span>$2')
			.replace(/:\s*&quot;([^&]*?)&quot;/g, ': <span class="s">&quot;$1&quot;</span>')
			.replace(/:\s*(-?\d+(?:\.\d+)?)/g, ': <span class="n">$1</span>');
	}

	const argsHtml = $derived(tintJson(args));
</script>

{#if (args && args !== '{}') || result || diagnostics}
	<div class="raw" class:open>
		<button class="toggle" onclick={() => (open = !open)} aria-expanded={open}>
			<svg
				class="chev"
				viewBox="0 0 12 12"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				stroke-linecap="round"
				stroke-linejoin="round"
			>
				<polyline points="4,2 8,6 4,10" />
			</svg>
			<span class="label"
				>debug · args{result ? ' + result' : ''}{diagnostics ? ' + diagnostics' : ''}</span
			>
		</button>
		{#if open}
			<div class="body">
				{#if args && args !== '{}'}
					<!-- eslint-disable-next-line svelte/no-at-html-tags -->
					<pre class="pre">{@html argsHtml}</pre>
				{/if}
				{#if result}
					<div class="section-label">result</div>
					<pre class="pre">{result}</pre>
				{/if}
				{#if diagnostics}
					<div class="section-label">
						diagnostics <span class="cn">· sent to the model (not shown above)</span>
					</div>
					<pre class="pre">{diagnostics}</pre>
				{/if}
			</div>
		{/if}
	</div>
{/if}

<style>
	.raw {
		border-top: 1px solid var(--border-subtle);
	}
	.toggle {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-4);
		background: transparent;
		border: none;
		cursor: pointer;
		text-align: left;
		color: var(--text-tertiary);
	}
	.toggle:hover {
		color: var(--text-secondary);
	}
	.chev {
		width: 11px;
		height: 11px;
		flex-shrink: 0;
		transition: transform var(--dur-std) var(--ease-out);
	}
	.raw.open .chev {
		transform: rotate(90deg);
	}
	.label {
		font-size: 10px;
		font-weight: 510;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		font-family: var(--font-mono);
	}
	.body {
		padding: 0 var(--space-4) var(--space-3);
	}
	.section-label {
		font-size: 10px;
		font-weight: 510;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-tertiary);
		margin-top: var(--space-2);
	}
	/* Chinese annotation inside a section label: per DESIGN.md §3 Chinese must
	   use --font-chinese, never the Inter fallback; undo the label's latin
	   uppercase/tracking so the note reads naturally. */
	.section-label .cn {
		font-family: var(--font-chinese);
		text-transform: none;
		letter-spacing: 0;
	}
	.pre {
		margin: 0;
		font-family: var(--font-mono);
		font-size: 11.5px;
		color: var(--text-secondary);
		line-height: 1.6;
		white-space: pre;
		overflow-x: auto;
		max-height: 200px;
	}
	.pre :global(.k) {
		color: var(--syntax-key);
	}
	.pre :global(.s) {
		color: var(--syntax-str);
	}
	.pre :global(.n) {
		color: var(--syntax-num);
	}
</style>
