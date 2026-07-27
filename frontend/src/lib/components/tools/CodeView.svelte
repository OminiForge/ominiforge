<script lang="ts">
	import { browser } from '$app/environment';
	import { onMount } from 'svelte';
	import { highlighter, langFromPath, captureToClass } from '$lib/tools/highlight-ts';

	/** Renders a `read` file body: a `[path]` chip over a numbered gutter beside
	 *  the syntax-highlighted source. `lines` are the raw `N:content` rows from
	 *  the tool output (header already stripped by the caller).
	 *
	 *  The body is highlighted as ONE block (not per-line) so multi-line
	 *  constructs don't break; the gutter is a separate non-selectable `<pre>` so
	 *  a copy grabs only code. Only the tree-sitter markup (itself pre-escaped)
	 *  reaches `{@html}`. Alternatively pass raw `code` (e.g. a `write` new-file
	 *  view, `doc/tool-view.md`) — it is numbered 1..N itself. */
	let { path, lines, code }: { path: string; lines?: string[]; code?: string } = $props();

	interface Split {
		nums: string;
		code: string;
	}

	const split = $derived.by<Split>(() => {
		const nums: string[] = [];
		const codes: string[] = [];
		if (code !== undefined) {
			code.split('\n').forEach((l, i) => {
				nums.push(String(i + 1));
				codes.push(l);
			});
			return { nums: nums.join('\n'), code: codes.join('\n') };
		}
		for (const l of lines ?? []) {
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

	/** Highlight the code with tree-sitter, returning escaped HTML with spans
	 *  mapped to design-token classes. Falls back to plain escaped text when the
	 *  grammar is unavailable or tree-sitter fails. */
	async function highlightTs(codeText: string, lang: string): Promise<string> {
		const spans = await highlighter.highlight(codeText, lang, path);
		if (spans.length === 0) return escapeHtml(codeText);

		let html = '';
		let last = 0;
		for (const span of spans) {
			// Emit plain text before the span, then the span itself.
			if (span.start > last) {
				html += escapeHtml(codeText.slice(last, span.start));
			}
			const cls = captureToClass(span.capture);
			html += `<span class="${cls}">${escapeHtml(codeText.slice(span.start, span.end))}</span>`;
			last = span.end;
		}
		if (last < codeText.length) {
			html += escapeHtml(codeText.slice(last));
		}
		return html;
	}

	let bodyHtml = $state('');

	$effect(() => {
		bodyHtml = escapeHtml(split.code);
	});

	onMount(() => {
		if (!browser) return;
		const lang = langFromPath(path);
		if (!lang) return;
		let cancelled = false;
		void highlightTs(split.code, lang).then((html) => {
			// Don't write to state after the component is gone (rapid unmount
			// while the WASM grammar was still loading).
			if (!cancelled) bodyHtml = html;
		});
		return () => {
			cancelled = true;
		};
	});
</script>

<div class="head">
	<span class="path">{path}</span>
</div>
<div class="wrap">
	<pre class="gutter" aria-hidden="true">{split.nums}</pre>
	<!-- eslint-disable-next-line svelte/no-at-html-tags -->
	<pre class="code"><code>{@html bodyHtml}</code></pre>
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

	/* tree-sitter capture → design token (scoped). The classes are emitted by
	   `captureToClass` in `highlight-ts.ts`. */
	.code :global(.syntax-comment) {
		color: var(--syntax-comment);
		font-style: italic;
	}
	.code :global(.syntax-keyword) {
		color: var(--syntax-keyword);
	}
	.code :global(.syntax-str) {
		color: var(--syntax-str);
	}
	.code :global(.syntax-num) {
		color: var(--syntax-num);
	}
	.code :global(.syntax-fn) {
		color: var(--syntax-fn);
	}
	.code :global(.syntax-type) {
		color: var(--syntax-type);
	}
	.code :global(.syntax-key) {
		color: var(--syntax-key);
	}
</style>
