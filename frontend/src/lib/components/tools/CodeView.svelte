<script lang="ts">
	import { browser } from '$app/environment';
	import { highlightBlock } from '$lib/tools/highlight';

	/** Renders a `read` file body: a `[path]` chip over a numbered gutter beside
	 *  the syntax-highlighted source. `lines` are the raw `N:content` rows from
	 *  the tool output (header already stripped by the caller).
	 *
	 *  The body is highlighted as ONE block (not per-line) so multi-line hljs
	 *  constructs don't break; the gutter is a separate non-selectable `<pre>` so
	 *  a copy grabs only code. Only the hljs markup (itself pre-escaped) reaches
	 *  `{@html}`. */
	let { path, lines }: { path: string; lines: string[] } = $props();

	interface Split {
		nums: string;
		code: string;
	}

	const split = $derived.by<Split>(() => {
		const nums: string[] = [];
		const codes: string[] = [];
		for (const l of lines) {
			const m = /^(\d+):([\s\S]*)$/.exec(l);
			if (m) {
				nums.push(m[1]);
				codes.push(m[2]);
			} else {
				nums.push('');
				codes.push(l);
			}
		}
		return { nums: nums.join('\n'), code: codes.join('\n') };
	});

	function escapeHtml(s: string): string {
		return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
	}

	const bodyHtml = $derived(browser ? highlightBlock(split.code, path) : escapeHtml(split.code));
</script>

<div class="head">
	<span class="path">{path}</span>
</div>
<div class="wrap">
	<pre class="gutter" aria-hidden="true">{split.nums}</pre>
	<!-- eslint-disable-next-line svelte/no-at-html-tags -->
	<pre class="code"><code class="hljs">{@html bodyHtml}</code></pre>
</div>

<style>
	.head {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
		padding-bottom: var(--space-2);
		margin-bottom: var(--space-2);
		border-bottom: 1px solid var(--border-subtle);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		font-family: var(--font-mono);
		font-size: 11.5px;
	}
	.path {
		color: var(--accent-ink);
		font-weight: 500;
	}
	.wrap {
		display: flex;
		gap: var(--space-3);
		align-items: flex-start;
		font-family: var(--font-mono);
		font-size: 11.5px;
		line-height: 1.6;
	}
	.gutter {
		margin: 0;
		font: inherit;
		color: var(--text-disabled);
		text-align: right;
		user-select: none;
		flex-shrink: 0;
		font-variant-numeric: tabular-nums;
		white-space: pre;
	}
	.code {
		margin: 0;
		flex: 1;
		min-width: 0;
		overflow-x: auto;
	}
	.code code {
		font: inherit;
		color: var(--text-secondary);
		white-space: pre;
	}

	/* hljs token → design token (scoped). Covers the classes that appear in the
	   source files this repo reads: keywords, types, attributes/decorators,
	   operators, template substitutions. */
	.code :global(.hljs-comment),
	.code :global(.hljs-quote) {
		color: var(--syntax-comment);
		font-style: italic;
	}
	.code :global(.hljs-keyword),
	.code :global(.hljs-selector-tag),
	.code :global(.hljs-literal),
	.code :global(.hljs-section),
	.code :global(.hljs-doctag),
	.code :global(.hljs-name) {
		color: var(--syntax-keyword);
	}
	.code :global(.hljs-string),
	.code :global(.hljs-regexp),
	.code :global(.hljs-char),
	.code :global(.hljs-char.escape_),
	.code :global(.hljs-selector-attr),
	.code :global(.hljs-selector-pseudo),
	.code :global(.hljs-meta .hljs-string) {
		color: var(--syntax-str);
	}
	.code :global(.hljs-number),
	.code :global(.hljs-bullet) {
		color: var(--syntax-num);
	}
	.code :global(.hljs-title),
	.code :global(.hljs-title.function_),
	.code :global(.hljs-function .hljs-title),
	.code :global(.hljs-built_in) {
		color: var(--syntax-fn);
	}
	.code :global(.hljs-type),
	.code :global(.hljs-title.class_),
	.code :global(.hljs-class .hljs-title),
	.code :global(.hljs-attr),
	.code :global(.hljs-attribute),
	.code :global(.hljs-property),
	.code :global(.hljs-selector-class),
	.code :global(.hljs-selector-id) {
		color: var(--syntax-type);
	}
	.code :global(.hljs-variable),
	.code :global(.hljs-template-variable),
	.code :global(.hljs-params),
	.code :global(.hljs-symbol) {
		color: var(--syntax-key);
	}
	.code :global(.hljs-meta),
	.code :global(.hljs-meta .hljs-keyword) {
		color: var(--syntax-fn);
	}
	.code :global(.hljs-operator),
	.code :global(.hljs-punctuation) {
		color: var(--text-tertiary);
	}
	.code :global(.hljs-subst) {
		color: var(--text-secondary);
	}
	.code :global(.hljs-emphasis) {
		font-style: italic;
	}
	.code :global(.hljs-strong) {
		font-weight: 600;
	}
</style>
