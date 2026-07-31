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
	 *  view, `doc/tool-view.md`) — it is numbered 1..N itself.
	 *
	 *  A ranged `read` view carries the WHOLE file in `code` plus `numbered:
	 *  true` and the resolved 1-based inclusive `start`/`end` window: the
	 *  highlight runs over the full document (a partial file breaks multi-line
	 *  constructs) and the display slices to the window, gutter numbered by the
	 *  window's absolute lines. Legacy ranged views (pre-window, `code` is the
	 *  slice as `N:line` rows) are parsed like `lines`. */
	let {
		path,
		lines,
		code,
		numbered,
		start,
		end
	}: {
		path: string;
		lines?: string[];
		code?: string;
		numbered?: boolean;
		start?: number;
		end?: number;
	} = $props();

	interface Split {
		nums: string;
		code: string;
	}

	const split = $derived.by<Split>(() => {
		const nums: string[] = [];
		const codes: string[] = [];
		if (code !== undefined && !numbered) {
			code.split('\n').forEach((l, i) => {
				nums.push(String(i + 1));
				codes.push(l);
			});
			return { nums: nums.join('\n'), code: codes.join('\n') };
		}
		if (code !== undefined && numbered && start !== undefined && end !== undefined) {
			// Windowed view: `code` is the WHOLE file; display lines start..end
			// (1-based inclusive) numbered absolutely. The full text is what gets
			// highlighted (see `fullCode` below).
			const all = code.split('\n');
			for (let n = start; n <= end && n <= all.length; n++) {
				nums.push(String(n));
				codes.push(all[n - 1]);
			}
			return { nums: nums.join('\n'), code: codes.join('\n') };
		}
		// Legacy ranged view: `code` (or `lines`) holds `N:line` rows.
		for (const l of numbered && code !== undefined ? code.split('\n') : (lines ?? [])) {
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

	/** What the highlighter parses: the WHOLE document for a windowed view
	 *  (slicing before parsing would break multi-line constructs), otherwise
	 *  exactly what's displayed. */
	const fullCode = $derived(
		code !== undefined && numbered && start !== undefined && end !== undefined ? code : split.code
	);

	function escapeHtml(s: string): string {
		return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
	}

	/** Offset of the first character of 1-based `line` in `text`. */
	function lineOffset(text: string, line: number): number {
		if (line <= 1) return 0;
		let off = 0;
		for (let i = 1; i < line; i++) {
			const nl = text.indexOf('\n', off);
			if (nl === -1) return text.length;
			off = nl + 1;
		}
		return off;
	}

	/** Highlight the code with tree-sitter, returning escaped HTML with spans
	 *  mapped to design-token classes. A windowed view (`start`/`end` set) is
	 *  parsed whole but only the window's lines are emitted. Falls back to
	 *  plain escaped text when the grammar is unavailable or tree-sitter
	 *  fails. */
	async function highlightTs(codeText: string, lang: string): Promise<string> {
		const spans = await highlighter.highlight(codeText, lang, path);
		const windowed = start !== undefined && end !== undefined && numbered && code !== undefined;
		if (!windowed) {
			if (spans.length === 0) return escapeHtml(codeText);
			return emitSpans(codeText, 0, codeText.length, spans);
		}
		const lo = lineOffset(codeText, start!);
		const hi = lineOffset(codeText, end! + 1);
		// Strip the trailing newline of the window's last line for display.
		const displayEnd = hi > lo && codeText[hi - 1] === '\n' ? hi - 1 : hi;
		if (spans.length === 0) return escapeHtml(codeText.slice(lo, displayEnd));
		return emitSpans(codeText, lo, displayEnd, spans);
	}

	/** Emit escaped HTML for `text[from..to]`, wrapping highlight spans that
	 *  intersect it (clipped at the window edges). */
	function emitSpans(
		text: string,
		from: number,
		to: number,
		spans: { start: number; end: number; capture: string }[]
	): string {
		let html = '';
		let last = from;
		for (const span of spans) {
			const s = Math.max(span.start, from);
			const e = Math.min(span.end, to);
			if (e <= last) continue;
			if (s > last) html += escapeHtml(text.slice(last, s));
			const cls = captureToClass(span.capture);
			html += `<span class="${cls}">${escapeHtml(text.slice(Math.max(s, last), e))}</span>`;
			last = e;
		}
		if (last < to) html += escapeHtml(text.slice(last, to));
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
		void highlightTs(fullCode, lang).then((html) => {
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
	{#if numbered}<span class="range">:{start ?? 1}–{end ?? '?'}</span>{/if}
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
	.range {
		color: var(--text-disabled);
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
